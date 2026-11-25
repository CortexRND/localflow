//! Models & keys page (the native half of `localflow/hub_mac.py`).
//!
//! The compact Change models card, import sheet, provider rows/editors and the
//! Free up space card all use real AppKit controls. Draft URLs, model IDs and
//! import ticks live in [`State`], not in views, so a rebuild cannot lose them.
//! A secure key field is deliberately never captured or represented in page
//! state. Key status and key writes go through [`lf_providers::ProviderKeys`];
//! import rows contain only [`ImportCandidate::masked_value`].
//!
//! Ollama model lists and provider health checks are blocking network calls.
//! They run on worker threads and return through an mpsc channel drained by the
//! main-run-loop timer. Each request has a token, so a late response cannot
//! overwrite newer state.
//! Snapshot seeding installs a no-external-access backend and starts no worker.
#![allow(deprecated)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::{mpsc, Arc};

use anyhow::{anyhow, Result};
use lf_core::config::{load_config, load_from, Config};
use lf_providers::keys::{
    detect_imports, get_preset, ImportCandidate, KeyStatus, Preset, TEST_UNABLE,
};
use lf_providers::ollama::{OllamaLlm, DEFAULT_NUM_CTX};
use lf_providers::{ConfigStore, CoreSecrets, LlmProvider, ModelInfo, ProviderKeys, PRESETS};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAccessibility, NSAlert, NSAlertFirstButtonReturn, NSButton, NSComboBox,
    NSControlStateValueOff, NSControlStateValueOn, NSModalResponseOK, NSOpenPanel, NSPopUpButton,
    NSSecureTextField, NSTextField, NSView,
};
use objc2_foundation::{NSArray, NSPoint, NSString};
use serde_json::Value;

use crate::mac::actions::Timer;
use crate::mac::page::{HubPage, HubServices, Measure, PageCtx};
use crate::mac::style::{
    color, hide_from_accessibility, make_pill, rect, rect_of, style_surface, ButtonKind,
    FlippedView, SurfaceStyle,
};
use crate::mac::widgets::{button, field, label, popup, LabelStyle};
use crate::models_layout::{
    action_frames, active_preset, cleanup_picker, current_speech_provider, editor_layout,
    import_height, import_states, is_active, list_layout, note_lines, provider_status,
    provider_status_text, quick_layout, request_retry, row_columns, set_active_model, set_speech,
    show_use_button, speech_model_choices, speech_provider_choices, speech_summary,
    validate_model_id, EditorRows, ImportKey, ListState, ModelChoiceError, ACTION_H, CLEANUP_LABEL,
    IMPORT_HEADER_H, IMPORT_ROW_H, OLLAMA_LIST_TIMEOUT,
};
use crate::pystr::PyStr;
use crate::tokens::{
    Palette, Weight, CARD_BORDER_WIDTH, CARD_PADDING, HUB_PAGE_BOTTOM, LIGHT, MODELS_ROW_HEIGHT,
    PILL_HEIGHT,
};

const PAGE: &str = "Models";
const SAVE_FAILED: &str = "Unable to save; check the key, URL, and Keychain access";
const IMPORT_SAVE_FAILED: &str = "Unable to save keys; check Keychain access";
const CONFIG_SAVE_FAILED: &str =
    "Couldn't save the model; check that LocalFlow's settings can be written.";
const CONFIG_LOAD_FAILED: &str =
    "Unable to read model settings; check that LocalFlow's settings can be read.";
const SPEECH_SAVE_FAILED: &str =
    "Couldn't save the speech model; check that LocalFlow's settings can be written.";
const STORAGE_READ_FAILED: &str = "Couldn't read the model cache.";
const ROW_H: f64 = 36.0;
const STORAGE_TOP: f64 = 16.0 + 22.0 + 20.0 + 8.0;

// ------------------------------------------------------------------ backend ---

/// A cached speech model, safe to expose to snapshots (there are no credentials
/// or absolute paths in model metadata).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedModel {
    pub repo: String,
    pub label: String,
    pub size_bytes: u64,
    /// `"In use"`, `"Selected"`, `"Unknown use"`, or empty (deletable).
    pub status: String,
}

impl CachedModel {
    /// Metadata-only model for a populated snapshot or a fake backend.
    pub fn new(repo: &str, label: &str, size_bytes: u64, status: &str) -> Self {
        Self {
            repo: repo.to_string(),
            label: label.to_string(),
            size_bytes,
            status: status.to_string(),
        }
    }

    pub fn protected(&self) -> bool {
        !self.status.is_empty()
    }
}

/// Machine-dependent page operations. Tests can implement only the operations
/// they exercise; defaults neither access external state nor claim success.
/// Secret values never cross this interface except for `save_key`, whose value
/// is passed directly from a secure field to `ProviderKeys`.
pub trait ModelsBackend: Send + Sync {
    fn key_status(&self, provider_id: &str) -> Result<KeyStatus> {
        let _ = provider_id;
        Ok(KeyStatus {
            has_key: false,
            masked_value: String::new(),
        })
    }

    fn key_needs_activation(&self, provider_id: &str, config: &Config) -> Result<bool> {
        let _ = (provider_id, config);
        Ok(false)
    }

    fn save_key(&self, provider_id: &str, value: &str) -> Result<()> {
        let _ = (provider_id, value);
        Err(anyhow!("provider key storage is unavailable"))
    }

    fn remove_key(&self, provider_id: &str, config: &Config) -> Result<()> {
        let _ = (provider_id, config);
        Err(anyhow!("provider key storage is unavailable"))
    }

    fn save_imports(&self, candidates: &[ImportCandidate]) -> Result<()> {
        let _ = candidates;
        Err(anyhow!("provider key storage is unavailable"))
    }

    fn activate(
        &self,
        provider_id: &str,
        base_url: &str,
        model: &str,
        config: &Config,
        save: &dyn Fn(&Config) -> Result<()>,
    ) -> Result<Config> {
        let _ = (provider_id, base_url, model, config, save);
        Err(anyhow!("provider activation is unavailable"))
    }

    fn list_ollama(&self, base_url: &str) -> Result<Vec<ModelInfo>> {
        let _ = base_url;
        Err(anyhow!("Ollama listing is unavailable"))
    }

    fn fetch_models(&self, provider_id: &str, base_url: &str) -> Vec<ModelInfo> {
        let _ = (provider_id, base_url);
        Vec::new()
    }

    fn test_llm(&self, provider_id: &str, base_url: &str, model: &str) -> (bool, String) {
        let _ = (provider_id, base_url, model);
        (false, TEST_UNABLE.to_string())
    }

    /// No speech runtime is linked into `lf-ui`; an available runtime must be
    /// explicitly supplied by an injected backend. `auto` is offered by the
    /// shared layout rules regardless of this callback.
    fn speech_available(&self, provider_id: &str) -> bool {
        let _ = provider_id;
        false
    }

    fn inventory(&self, config: &Config) -> Result<Vec<CachedModel>> {
        let _ = config;
        Ok(Vec::new())
    }

    fn delete_model(&self, repo: &str, config: &Config) -> Result<u64> {
        let _ = (repo, config);
        Err(anyhow!("speech model storage is unavailable"))
    }
}

/// Adapter that makes provider activation save through the Hub's configured
/// destination, including memory-only snapshot/test services.
struct PageConfigStore<'a> {
    current: Config,
    save: &'a dyn Fn(&Config) -> Result<()>,
}

impl ConfigStore<Config> for PageConfigStore<'_> {
    fn load(&self) -> Result<Config> {
        Ok(self.current.clone())
    }

    fn save(&self, config: &Config) -> Result<()> {
        (self.save)(config)
    }
}

/// The production backend: real keyring/config/provider clients and a guarded
/// Hugging Face speech-model inventory. `cache` and `leases` are injectable
/// paths so its filesystem tests never inspect the user's cache or processes.
pub struct SystemModelsBackend {
    cache: Option<PathBuf>,
    leases: Option<PathBuf>,
    /// Python's optional MLX runtime may be installed outside this process.
    mlx_available: bool,
}

impl Default for SystemModelsBackend {
    fn default() -> Self {
        Self {
            cache: None,
            leases: None,
            // Keep both auto candidates on Apple Silicon, as lf-stt does.
            mlx_available: cfg!(target_arch = "aarch64"),
        }
    }
}

impl ModelsBackend for SystemModelsBackend {
    fn key_status(&self, provider_id: &str) -> Result<KeyStatus> {
        ProviderKeys::system().key_status(provider_id)
    }

    fn key_needs_activation(&self, provider_id: &str, config: &Config) -> Result<bool> {
        ProviderKeys::system().key_needs_activation(provider_id, Some(config))
    }

    fn save_key(&self, provider_id: &str, value: &str) -> Result<()> {
        ProviderKeys::system().save_key(provider_id, value)
    }

    fn remove_key(&self, provider_id: &str, config: &Config) -> Result<()> {
        ProviderKeys::system().remove_key(provider_id, Some(config))
    }

    fn save_imports(&self, candidates: &[ImportCandidate]) -> Result<()> {
        ProviderKeys::system().save_imports(candidates)
    }

    fn activate(
        &self,
        provider_id: &str,
        base_url: &str,
        model: &str,
        config: &Config,
        save: &dyn Fn(&Config) -> Result<()>,
    ) -> Result<Config> {
        let secrets = CoreSecrets;
        let store = PageConfigStore {
            current: config.clone(),
            save,
        };
        ProviderKeys::new(&secrets, &store).activate(
            provider_id,
            Some(base_url),
            Some(model),
            Some(config.clone()),
        )
    }

    fn list_ollama(&self, base_url: &str) -> Result<Vec<ModelInfo>> {
        OllamaLlm::new(base_url, "", OLLAMA_LIST_TIMEOUT, DEFAULT_NUM_CTX)
            .list_models()
            .map_err(|error| anyhow!(error.0))
    }

    fn fetch_models(&self, provider_id: &str, base_url: &str) -> Vec<ModelInfo> {
        ProviderKeys::system().fetch_models(provider_id, Some(base_url))
    }

    fn test_llm(&self, provider_id: &str, base_url: &str, model: &str) -> (bool, String) {
        ProviderKeys::system().test_llm(provider_id, Some(base_url), model)
    }

    fn inventory(&self, config: &Config) -> Result<Vec<CachedModel>> {
        storage_inventory(
            config,
            self.cache.as_deref(),
            self.leases.as_deref(),
            self.mlx_available,
        )
    }

    fn delete_model(&self, repo: &str, config: &Config) -> Result<u64> {
        storage_delete(
            repo,
            config,
            self.cache.as_deref(),
            self.leases.as_deref(),
            self.mlx_available,
        )
    }
}

// --------------------------------------------------------- safe public state ---

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelsMessage {
    pub text: String,
    pub success: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderSnapshot {
    pub id: String,
    pub url: String,
    pub model: String,
    pub models: Vec<String>,
    pub message: ModelsMessage,
}

/// Minimal, credential-free state for empty/populated/editor/error snapshots.
/// Import values are intentionally absent; use `seed_import_candidates` when an
/// import-results snapshot is needed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelsSnapshot {
    pub expanded: Option<String>,
    pub providers: Vec<ProviderSnapshot>,
    pub statuses: BTreeMap<String, KeyStatus>,
    pub quick_models: Vec<String>,
    pub quick_list_state: Option<ListState>,
    pub quick_draft: Option<String>,
    pub quick_message: ModelsMessage,
    pub provider_message: ModelsMessage,
    pub storage: Vec<CachedModel>,
    pub storage_message: ModelsMessage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportSnapshot {
    pub provider_id: String,
    pub source: String,
    pub masked_value: String,
    pub selected: bool,
}

#[derive(Default)]
struct ProviderState {
    url: String,
    model: String,
    models: Vec<String>,
    loaded: Option<(String, String)>,
    message: ModelsMessage,
    models_token: u64,
    test_token: u64,
}

#[derive(Default)]
struct QuickState {
    message: ModelsMessage,
    draft: Option<String>,
    models: Vec<String>,
    list_state: Option<ListState>,
    ollama_url: Option<String>,
    token: u64,
    drawn_for: Option<(String, String)>,
    speech: Vec<String>,
    sizes: Vec<String>,
    cleanup: Vec<String>,
}

struct EditorControls {
    provider_id: String,
    url: Option<Retained<NSTextField>>,
    combo: Retained<NSComboBox>,
    /// Live only. Never copied into `ProviderState` or a snapshot.
    secure: Option<Retained<NSSecureTextField>>,
    use_button: Retained<NSButton>,
    test: Retained<NSButton>,
    rows: EditorRows,
}

#[derive(Default)]
struct QuickControls {
    cleanup_model: Option<Retained<NSPopUpButton>>,
    cleanup_field: Option<Retained<NSTextField>>,
    refresh: Option<Retained<NSButton>>,
    note: Option<Retained<NSTextField>>,
}

#[derive(Clone)]
struct QuickGeometry {
    body: Retained<FlippedView>,
    card: Retained<FlippedView>,
    width: f64,
    viewport_height: f64,
    ollama: bool,
    fixed: bool,
}

#[derive(Default)]
struct State {
    backend: Option<Arc<dyn ModelsBackend>>,
    providers: BTreeMap<String, ProviderState>,
    expanded: Option<String>,
    provider_message: ModelsMessage,
    candidates: Vec<ImportCandidate>,
    checked: BTreeMap<ImportKey, bool>,
    quick: QuickState,
    storage: Vec<CachedModel>,
    storage_message: ModelsMessage,
    statuses: BTreeMap<String, KeyStatus>,
    /// Snapshot-only, credential-free overrides for real machine state.
    seeded_statuses: Option<BTreeMap<String, KeyStatus>>,
    seeded_storage: Option<Vec<CachedModel>>,
    editor: Option<EditorControls>,
    imports: Vec<Retained<NSButton>>,
    quick_controls: QuickControls,
    quick_geometry: Option<QuickGeometry>,
    services: Option<Rc<HubServices>>,
    rebuild: Option<Rc<dyn Fn()>>,
    palette: Option<Palette>,
    measure: Option<Measure>,
    mtm: Option<MainThreadMarker>,
    worker_sender: Option<mpsc::Sender<Completion>>,
    worker_receiver: Option<mpsc::Receiver<Completion>>,
    poll_timer: Option<Timer>,
    visible: bool,
    closed: bool,
    suspend_workers: bool,
    system_backend: bool,
}

enum Completion {
    Quick {
        token: u64,
        models: Vec<String>,
        listing: ListState,
    },
    ProviderModels {
        provider_id: String,
        token: u64,
        models: Vec<String>,
    },
    ProviderTest {
        provider_id: String,
        token: u64,
        result: (bool, String),
    },
}

type Shared = Rc<RefCell<State>>;

/// The native Models & keys page.
pub struct ModelsPage {
    state: Shared,
}

impl Default for ModelsPage {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelsPage {
    /// A production page using the real `lf-providers` keyring and clients.
    pub fn new() -> Self {
        let page = Self::with_backend(Arc::new(SystemModelsBackend::default()));
        page.state.borrow_mut().system_backend = true;
        page
    }

    /// Inject machine capabilities. No real keychain or provider client is
    /// consulted by tests that supply a fake backend.
    pub fn with_backend(backend: Arc<dyn ModelsBackend>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let state = State {
            backend: Some(backend),
            worker_sender: Some(sender),
            worker_receiver: Some(receiver),
            ..State::default()
        };
        Self {
            state: Rc::new(RefCell::new(state)),
        }
    }

    /// Read the credential-free state used by native snapshot drivers.
    pub fn snapshot_state(&self) -> ModelsSnapshot {
        let state = self.state.borrow();
        ModelsSnapshot {
            expanded: state.expanded.clone(),
            providers: PRESETS
                .iter()
                .map(|preset| {
                    let provider = state.providers.get(preset.id);
                    ProviderSnapshot {
                        id: preset.id.to_string(),
                        url: provider
                            .map_or(preset.base_url, |p| p.url.as_str())
                            .to_string(),
                        model: provider
                            .map_or(preset.default_model, |p| p.model.as_str())
                            .to_string(),
                        models: provider.map_or_else(Vec::new, |p| p.models.clone()),
                        message: provider
                            .map_or_else(ModelsMessage::default, |p| p.message.clone()),
                    }
                })
                .collect(),
            statuses: state.statuses.clone(),
            quick_models: state.quick.models.clone(),
            quick_list_state: state.quick.list_state,
            quick_draft: state.quick.draft.clone(),
            quick_message: state.quick.message.clone(),
            provider_message: state.provider_message.clone(),
            storage: state.storage.clone(),
            storage_message: state.storage_message.clone(),
        }
    }

    /// Seed any empty/populated/editor/error state without real config/keychain
    /// or network access. No unsaved key can be supplied, by design.
    pub fn seed_snapshot(&self, seed: ModelsSnapshot) {
        let mut state = self.state.borrow_mut();
        // A fresh page previously held the production backend. Replace it so a
        // snapshot action can never touch real credentials either.
        state.backend = Some(Arc::new(NoExternalBackend));
        state.suspend_workers = true;
        state.poll_timer = None;
        state.quick.token = state.quick.token.wrapping_add(1);
        for provider in state.providers.values_mut() {
            provider.models_token = provider.models_token.wrapping_add(1);
            provider.test_token = provider.test_token.wrapping_add(1);
        }
        state.expanded = seed
            .expanded
            .filter(|id| PRESETS.iter().any(|preset| preset.id == id));
        state.providers = seed
            .providers
            .into_iter()
            .filter(|provider| PRESETS.iter().any(|preset| preset.id == provider.id))
            .map(|provider| {
                (
                    provider.id,
                    ProviderState {
                        url: provider.url,
                        model: provider.model,
                        models: provider.models,
                        message: provider.message,
                        ..ProviderState::default()
                    },
                )
            })
            .collect();
        state.seeded_statuses = Some(seed.statuses.clone());
        state.statuses = seed.statuses;
        state.quick.models = seed.quick_models;
        state.quick.list_state = Some(seed.quick_list_state.unwrap_or(ListState::Empty));
        state.quick.draft = seed.quick_draft;
        state.quick.message = seed.quick_message;
        state.provider_message = seed.provider_message;
        state.seeded_storage = Some(seed.storage.clone());
        state.storage = seed.storage;
        state.storage_message = seed.storage_message;
        state.candidates.clear();
        state.checked.clear();
        state.imports.clear();
        state.clear_controls();
    }

    /// Seed real import candidates created by `lf_providers::detect_imports`
    /// (usually from a synthetic environment map). Values stay inside the
    /// private candidate representation; snapshots expose only masked values.
    pub fn seed_import_candidates(&self, candidates: Vec<ImportCandidate>) {
        let mut state = self.state.borrow_mut();
        state.checked.clear();
        state.candidates = candidates;
    }

    /// Masked import metadata for asserting an import-results snapshot.
    pub fn import_snapshot(&self) -> Vec<ImportSnapshot> {
        let state = self.state.borrow();
        let ticks = import_states(
            &state.candidates,
            &|id| state.statuses.get(id).is_some_and(|status| status.has_key),
            &state.checked,
        );
        state
            .candidates
            .iter()
            .map(|candidate| ImportSnapshot {
                provider_id: candidate.provider_id.clone(),
                source: candidate.source.clone(),
                masked_value: candidate.masked_value(),
                selected: ticks
                    .get(&(candidate.provider_id.clone(), candidate.source.clone()))
                    .copied()
                    .unwrap_or(false),
            })
            .collect()
    }
}

struct NoExternalBackend;
impl ModelsBackend for NoExternalBackend {}

// ------------------------------------------------------------- config/saving ---

fn fresh_config(services: &HubServices) -> Result<Config> {
    if services.memory_only_config.get() {
        return Ok(services.config());
    }
    match services.config_path.borrow().as_deref() {
        Some(path) => load_from(path),
        None => load_config(),
    }
}

fn persist_config(services: &HubServices, before: &Config, config: &Config) -> Result<()> {
    if !services.memory_only_config.get() {
        crate::config_write::merge(services.config_path.borrow().as_deref(), before, config)?;
    }
    Ok(())
}

fn commit_config(services: &HubServices, config: Config) {
    *services.config.borrow_mut() = config;
}

fn provider_state<'a>(
    state: &'a mut State,
    preset: &Preset,
    config: &Config,
) -> &'a mut ProviderState {
    let mut initial_url = preset.base_url.to_string();
    let mut initial_model = preset.default_model.to_string();
    if is_active(preset, config, None) {
        if let Some(model) = config.field_str("llm_model") {
            if !model.is_empty() {
                initial_model = model.to_string();
            }
        }
        if preset.id == "custom" {
            initial_url = config.field_str("llm_base_url").unwrap_or("").to_string();
        }
    }
    let entry = state
        .providers
        .entry(preset.id.to_string())
        .or_insert(ProviderState {
            url: initial_url,
            model: initial_model,
            ..ProviderState::default()
        });
    if entry.model.is_empty() && is_active(preset, config, None) {
        entry.model = config.field_str("llm_model").unwrap_or("").to_string();
    }
    entry
}

fn use_visible(
    backend: &dyn ModelsBackend,
    preset: &Preset,
    config: &Config,
    url: &str,
    model: &str,
) -> bool {
    if model.py_strip().is_empty() {
        return false;
    }
    if show_use_button(preset, config, url, model, false) {
        return true;
    }
    backend
        .key_needs_activation(preset.id, config)
        .map(|changed| show_use_button(preset, config, url, model, changed))
        .unwrap_or(false)
}

fn apply_use(state: &RefCell<State>) {
    let controls = state.borrow().editor.as_ref().map(|editor| {
        (
            editor.use_button.clone(),
            editor.test.clone(),
            editor.rows.clone(),
        )
    });
    let Some((use_button, test, rows)) = controls else {
        return;
    };
    let show = {
        let state = state.borrow();
        let Some(services) = &state.services else {
            return;
        };
        let Some(editor) = &state.editor else { return };
        let Ok(preset) = get_preset(&editor.provider_id) else {
            return;
        };
        let Some(provider) = state.providers.get(&editor.provider_id) else {
            return;
        };
        let backend = state.backend.as_ref().expect("page backend");
        use_visible(
            backend.as_ref(),
            preset,
            &services.config(),
            &provider.url,
            &provider.model,
        )
    };
    if let Some((_use_frame, test_frame)) = action_frames(&rows, show) {
        use_button.setHidden(!show);
        test.setFrame(rect_of(test_frame));
    }
}

/// Read only non-secret controls. In particular this never even calls
/// `stringValue` on the secure field.
fn pull(state: &RefCell<State>) {
    let live = state.borrow().editor.as_ref().map(|editor| {
        (
            editor.provider_id.clone(),
            editor.url.clone(),
            editor.combo.clone(),
        )
    });
    if let Some((id, url, combo)) = live {
        let url = url.map(|field| field.stringValue().to_string());
        let model = combo.stringValue().to_string();
        let mut state = state.borrow_mut();
        if let Some(provider) = state.providers.get_mut(&id) {
            if let Some(url) = url {
                provider.url = url.py_strip().to_string();
            }
            provider.model = model.py_strip().to_string();
        }
    }
    let checks = {
        let state = state.borrow();
        state
            .imports
            .iter()
            .map(|check| check.state() == NSControlStateValueOn)
            .collect::<Vec<_>>()
    };
    if !checks.is_empty() {
        let mut state = state.borrow_mut();
        state.checked = state
            .candidates
            .iter()
            .zip(checks)
            .map(|(candidate, checked)| {
                (
                    (candidate.provider_id.clone(), candidate.source.clone()),
                    checked,
                )
            })
            .collect();
    }
    let cleanup = state
        .borrow()
        .quick_controls
        .cleanup_field
        .as_ref()
        .map(|field| field.stringValue().to_string());
    if let Some(typed) = cleanup {
        let mut state = state.borrow_mut();
        let current = state
            .services
            .as_ref()
            .and_then(|services| services.config().field_str("llm_model").map(str::to_string))
            .unwrap_or_default();
        if typed.py_strip() != current {
            state.quick.draft = Some(typed);
        }
    }
}

impl State {
    fn clear_controls(&mut self) {
        if let Some(editor) = &self.editor {
            if let Some(secure) = &editor.secure {
                secure.setStringValue(&NSString::from_str(""));
            }
        }
        self.editor = None;
        self.imports.clear();
        self.quick_controls = QuickControls::default();
        self.quick_geometry = None;
    }

    fn request_rebuild(&self) {
        if self.visible {
            if let Some(rebuild) = &self.rebuild {
                rebuild();
            }
        }
    }
}

// ----------------------------------------------------------- native workers ---

fn ensure_poll_timer(state: &Shared) {
    if state.borrow().suspend_workers || state.borrow().poll_timer.is_some() {
        return;
    }
    let weak: Weak<RefCell<State>> = Rc::downgrade(state);
    state.borrow_mut().poll_timer = Some(Timer::schedule(0.05, true, move || {
        let Some(state) = weak.upgrade() else {
            return;
        };
        let mut completed = Vec::new();
        {
            let state = state.borrow();
            let Some(receiver) = &state.worker_receiver else {
                return;
            };
            while let Ok(completion) = receiver.try_recv() {
                completed.push(completion);
            }
        }
        for completion in completed {
            match completion {
                Completion::Quick {
                    token,
                    models,
                    listing,
                } => quick_listed(&state, token, models, listing),
                Completion::ProviderModels {
                    provider_id,
                    token,
                    models,
                } => provider_models_listed(&state, &provider_id, token, models),
                Completion::ProviderTest {
                    provider_id,
                    token,
                    result,
                } => provider_test_finished(&state, &provider_id, token, result),
            }
        }
    }));
}

fn quick_fetch(state: &Shared, url: &str, force: bool) {
    let (token, backend, sender, suspended) = {
        let mut state = state.borrow_mut();
        if !force
            && state.quick.ollama_url.as_deref() == Some(url)
            && state.quick.list_state.is_some()
        {
            return;
        }
        state.quick.ollama_url = Some(url.to_string());
        state.quick.list_state = Some(ListState::Loading);
        state.quick.models.clear();
        state.quick.token = state.quick.token.wrapping_add(1);
        (
            state.quick.token,
            state.backend.as_ref().expect("page backend").clone(),
            state.worker_sender.as_ref().expect("worker sender").clone(),
            state.suspend_workers,
        )
    };
    if suspended {
        return;
    }
    let url = url.to_string();
    let spawned = std::thread::Builder::new()
        .name("localflow-ollama-models".to_string())
        .spawn(move || {
            let (models, listing) = match backend.list_ollama(&url) {
                Ok(infos) => {
                    let models = infos
                        .into_iter()
                        .filter_map(|info| validate_model_id(&info.id).ok())
                        .collect::<Vec<_>>();
                    let listing = if models.is_empty() {
                        ListState::Empty
                    } else {
                        ListState::Ready
                    };
                    (models, listing)
                }
                Err(_) => (Vec::new(), ListState::Unavailable),
            };
            let _ = sender.send(Completion::Quick {
                token,
                models,
                listing,
            });
        });
    if spawned.is_err() {
        quick_listed(state, token, Vec::new(), ListState::Unavailable);
    }
}

fn quick_listed(state: &Shared, token: u64, models: Vec<String>, listing: ListState) {
    let (popup, refresh, config, note_allowed) = {
        let mut state = state.borrow_mut();
        if state.closed || state.quick.token != token {
            return;
        }
        state.quick.models = models.clone();
        state.quick.list_state = Some(listing);
        let config = state
            .services
            .as_ref()
            .map(|services| services.config())
            .unwrap_or_default();
        let (items, _selected, _) = cleanup_picker(&config, listing, &models);
        state.quick.cleanup = items;
        (
            state.quick_controls.cleanup_model.clone(),
            state.quick_controls.refresh.clone(),
            config,
            state.quick.message.text.is_empty(),
        )
    };
    if let Some(refresh) = refresh {
        refresh.setEnabled(true);
    }
    let note = cleanup_picker(&config, listing, &models).2;
    if let Some(popup) = popup {
        let (items, selected, _) = cleanup_picker(&config, listing, &models);
        fill_popup(
            &popup,
            &items,
            items.iter().position(|item| *item == selected),
        );
    }
    if note_allowed {
        quick_reflow(state, &note);
    }
}

fn start_provider_models(state: &Shared, provider_id: &str) {
    let (url, token, backend, sender, suspended) = {
        let mut state = state.borrow_mut();
        let Some(provider) = state.providers.get_mut(provider_id) else {
            return;
        };
        provider.models_token = provider.models_token.wrapping_add(1);
        (
            provider.url.clone(),
            provider.models_token,
            state.backend.as_ref().expect("page backend").clone(),
            state.worker_sender.as_ref().expect("worker sender").clone(),
            state.suspend_workers,
        )
    };
    if suspended {
        return;
    }
    let id = provider_id.to_string();
    let spawned = std::thread::Builder::new()
        .name("localflow-provider-models".to_string())
        .spawn(move || {
            let models = backend
                .fetch_models(&id, &url)
                .into_iter()
                .map(|model| model.id)
                .collect::<Vec<_>>();
            let _ = sender.send(Completion::ProviderModels {
                provider_id: id,
                token,
                models,
            });
        });
    if spawned.is_err() {
        provider_models_listed(state, provider_id, token, Vec::new());
    }
}

fn provider_models_listed(state: &Shared, provider_id: &str, token: u64, models: Vec<String>) {
    let combo = {
        let mut state = state.borrow_mut();
        if state.closed {
            return;
        }
        let Some(provider) = state.providers.get_mut(provider_id) else {
            return;
        };
        if provider.models_token != token {
            return;
        }
        provider.models = models;
        if !state.visible {
            return;
        }
        state
            .editor
            .as_ref()
            .filter(|editor| editor.provider_id == provider_id)
            .map(|editor| editor.combo.clone())
    };
    if let Some(combo) = combo {
        // Filling in place preserves any secure key currently being typed.
        let typed = combo.stringValue();
        combo.removeAllItems();
        let models = state
            .borrow()
            .providers
            .get(provider_id)
            .map(|provider| provider.models.clone())
            .unwrap_or_default();
        // SAFETY: combo box object values are NSString instances.
        unsafe { combo.addItemsWithObjectValues(&combo_items(&models)) };
        combo.setStringValue(&typed);
    }
}

fn start_provider_test(state: &Shared, provider_id: &str) {
    let (url, model, token, backend, sender, suspended) = {
        let mut state = state.borrow_mut();
        let Some(provider) = state.providers.get_mut(provider_id) else {
            return;
        };
        provider.test_token = provider.test_token.wrapping_add(1);
        (
            provider.url.clone(),
            provider.model.clone(),
            provider.test_token,
            state.backend.as_ref().expect("page backend").clone(),
            state.worker_sender.as_ref().expect("worker sender").clone(),
            state.suspend_workers,
        )
    };
    if suspended {
        return;
    }
    let id = provider_id.to_string();
    let spawned = std::thread::Builder::new()
        .name("localflow-provider-test".to_string())
        .spawn(move || {
            let result = backend.test_llm(&id, &url, &model);
            let _ = sender.send(Completion::ProviderTest {
                provider_id: id,
                token,
                result,
            });
        });
    if spawned.is_err() {
        provider_test_finished(state, provider_id, token, (false, TEST_UNABLE.to_string()));
    }
}

fn provider_test_finished(state: &Shared, provider_id: &str, token: u64, result: (bool, String)) {
    let mut state = state.borrow_mut();
    if state.closed {
        return;
    }
    let Some(provider) = state.providers.get_mut(provider_id) else {
        return;
    };
    if provider.test_token != token {
        return;
    }
    provider.message = ModelsMessage {
        text: result.1,
        success: result.0,
    };
    state.request_rebuild();
}

// --------------------------------------------------------------- quick card ---

fn combo_items(values: &[String]) -> Retained<NSArray> {
    let values: Vec<Retained<AnyObject>> = values
        .iter()
        .map(|value| {
            let string = NSString::from_str(value);
            Retained::into_super(Retained::into_super(string))
        })
        .collect();
    NSArray::from_retained_slice(&values)
}

fn fill_popup(popup: &NSPopUpButton, titles: &[String], selected: Option<usize>) {
    popup.removeAllItems();
    if !titles.is_empty() {
        let titles: Vec<Retained<NSString>> = titles
            .iter()
            .map(|title| NSString::from_str(title))
            .collect();
        popup.addItemsWithTitles(&NSArray::from_retained_slice(&titles));
        if let Some(selected) = selected.filter(|index| *index < popup.numberOfItems() as usize) {
            popup.selectItemAtIndex(selected as isize);
        }
    }
    popup.setEnabled(!titles.is_empty());
}

fn note_line_count(ctx: &PageCtx, text: &str) -> usize {
    note_lines(
        &[text],
        ctx.width - 2.0 * f64::from(CARD_PADDING),
        &|line| (ctx.measure)(line, 12.0, Weight::Regular),
    )
}

fn quick_reflow(state: &Shared, text: &str) {
    let (geometry, note, palette, measure, mtm) = {
        let state = state.borrow();
        if !state.visible {
            return;
        }
        let (Some(geometry), Some(measure), Some(mtm)) = (
            state.quick_geometry.as_ref(),
            state.measure.as_ref(),
            state.mtm,
        ) else {
            return;
        };
        (
            geometry.clone(),
            state.quick_controls.note.clone(),
            state.palette.unwrap_or(LIGHT),
            measure.clone(),
            mtm,
        )
    };
    let lines = note_lines(
        &[text],
        geometry.width - 2.0 * f64::from(CARD_PADDING),
        &|line| measure(line, 12.0, Weight::Regular),
    );
    let frames = quick_layout(geometry.width, geometry.ollama, geometry.fixed, lines);
    let mut note = note;
    if let Some(frame) = frames.message {
        if let Some(existing) = &note {
            existing.setFrame(rect_of(frame));
            existing.setStringValue(&NSString::from_str(text));
        } else {
            let created = label(
                mtm,
                text,
                frame.0,
                frame.1,
                frame.2,
                LabelStyle {
                    size: 12.0,
                    color: "muted",
                    height: frame.3,
                    wrap: true,
                    ..LabelStyle::default()
                },
                &palette,
            );
            geometry.card.addSubview(&created);
            note = Some(created);
        }
    } else if let Some(existing) = note.take() {
        existing.removeFromSuperview();
    }
    state.borrow_mut().quick_controls.note = note;
    let old = geometry.card.frame();
    let delta = frames.height - old.size.height;
    if delta == 0.0 {
        return;
    }
    let old_bottom = old.origin.y + old.size.height;
    geometry.card.setFrame(rect(
        old.origin.x,
        old.origin.y,
        old.size.width,
        frames.height,
    ));
    for view in geometry.body.subviews().iter() {
        let card_view: &NSView = &geometry.card;
        if !std::ptr::eq(&*view, card_view) {
            let origin = view.frame().origin;
            if origin.y >= old_bottom {
                view.setFrameOrigin(NSPoint::new(origin.x, origin.y + delta));
            }
        }
    }
    let bottom = geometry
        .body
        .subviews()
        .iter()
        .map(|view| {
            let frame = view.frame();
            frame.origin.y + frame.size.height
        })
        .fold(0.0, f64::max);
    let body_frame = geometry.body.frame();
    geometry.body.setFrame(rect(
        0.0,
        0.0,
        body_frame.size.width,
        (bottom + f64::from(HUB_PAGE_BOTTOM)).max(geometry.viewport_height),
    ));
}

fn quick_save_cleanup(state: &Shared, model: String) {
    let (services, drawn_for) = {
        let state = state.borrow();
        let Some(services) = &state.services else {
            return;
        };
        (services.clone(), state.quick.drawn_for.clone())
    };
    let config = match fresh_config(&services) {
        Ok(config) => config,
        Err(_) => {
            state.borrow_mut().quick.message = ModelsMessage {
                text: CONFIG_SAVE_FAILED.to_string(),
                success: false,
            };
            state.borrow_mut().quick.draft = Some(model);
            state.borrow().request_rebuild();
            return;
        }
    };
    let expected = drawn_for
        .as_ref()
        .map(|(provider, url)| (provider.as_str(), url.as_str()));
    let saved = set_active_model(&model, &config, expected, &|updated| {
        persist_config(&services, &config, updated)
    });
    match saved {
        Ok(updated) => {
            let saved_model = updated.field_str("llm_model").unwrap_or("").to_string();
            commit_config(&services, updated.clone());
            let active = active_preset(&updated).map(|preset| preset.id);
            let combo = {
                let mut state = state.borrow_mut();
                state.quick.draft = None;
                state.quick.message = ModelsMessage {
                    text: format!("Cleanup & Ask now use {saved_model}; no restart needed."),
                    success: true,
                };
                if let Some(id) = active {
                    if let Some(provider) = state.providers.get_mut(id) {
                        provider.model = saved_model.clone();
                    }
                }
                // A deferred rebuild calls capture first. Keep the open editor
                // consistent so that capture cannot restore the old model.
                state
                    .editor
                    .as_ref()
                    .filter(|editor| active == Some(editor.provider_id.as_str()))
                    .map(|editor| editor.combo.clone())
            };
            if let Some(combo) = combo {
                combo.setStringValue(&NSString::from_str(&saved_model));
            }
        }
        Err(error) => {
            let friendly = error
                .downcast_ref::<ModelChoiceError>()
                .map_or(CONFIG_SAVE_FAILED, |error| error.0.as_str());
            let mut state = state.borrow_mut();
            state.quick.message = ModelsMessage {
                text: friendly.to_string(),
                success: false,
            };
            state.quick.draft = Some(model);
        }
    }
    state.borrow().request_rebuild();
}

fn quick_save_speech(state: &Shared, provider: &str, model: Option<&str>) {
    let (services, backend) = {
        let state = state.borrow();
        let Some(services) = &state.services else {
            return;
        };
        (
            services.clone(),
            state.backend.as_ref().expect("page backend").clone(),
        )
    };
    let config = match fresh_config(&services) {
        Ok(config) => config,
        Err(_) => {
            state.borrow_mut().quick.message = ModelsMessage {
                text: SPEECH_SAVE_FAILED.to_string(),
                success: false,
            };
            state.borrow().request_rebuild();
            return;
        }
    };
    let available = |id: &str| backend.speech_available(id);
    let config_path = services.config_path.borrow().clone();
    let saved = set_speech(
        provider,
        model,
        &config,
        &available,
        &|updated| persist_config(&services, &config, updated),
        &|updated| {
            if services.memory_only_config.get() {
                return;
            }
            if let Err(error) = request_retry(updated, config_path.as_deref()) {
                log::warn!("could not request a speech-model retry: {error}");
            }
        },
    );
    match saved {
        Ok(updated) => {
            let summary = speech_summary(&updated);
            commit_config(&services, updated);
            state.borrow_mut().quick.message = ModelsMessage {
                text: format!(
                    "Speech now uses {summary}. LocalFlow loads it in the background (it may \
                     download first) and keeps the current model until it is ready."
                ),
                success: true,
            };
        }
        Err(error) => {
            let friendly = error
                .downcast_ref::<ModelChoiceError>()
                .map_or(SPEECH_SAVE_FAILED, |error| error.0.as_str());
            state.borrow_mut().quick.message = ModelsMessage {
                text: friendly.to_string(),
                success: false,
            };
        }
    }
    state.borrow().request_rebuild();
}

fn render_quick(ctx: &PageCtx, state: &Shared, top: f64) -> f64 {
    let config = ctx.services.config();
    let ollama = config.field_str("llm_provider") == Some("ollama");
    if ollama {
        let url = config.field_str("llm_base_url").unwrap_or("").to_string();
        {
            let mut current = state.borrow_mut();
            if current.suspend_workers && current.quick.ollama_url.is_none() {
                current.quick.ollama_url = Some(url.clone());
            }
        }
        quick_fetch(state, &url, false);
    }
    let provider = current_speech_provider(&config);
    let fixed = provider == "parakeet";
    let (message, note, listing, models, draft) = {
        let state = state.borrow();
        (
            state.quick.message.clone(),
            if ollama {
                cleanup_picker(
                    &config,
                    state.quick.list_state.unwrap_or(ListState::Loading),
                    &state.quick.models,
                )
                .2
            } else {
                String::new()
            },
            state.quick.list_state,
            state.quick.models.clone(),
            state.quick.draft.clone(),
        )
    };
    let text = if message.text.is_empty() {
        note
    } else {
        message.text.clone()
    };
    let lines = note_line_count(ctx, &text);
    let frames = quick_layout(ctx.width, ollama, fixed, lines);
    let card = FlippedView::new(ctx.mtm, rect(0.0, top, ctx.width, frames.height));
    style_surface(&card, &ctx.palette, &SurfaceStyle::default());
    ctx.body.addSubview(&card);
    let add_label = |text: &str,
                     frame: (f64, f64, f64, f64),
                     size: f64,
                     token: &'static str,
                     weight: Weight| {
        card.addSubview(&label(
            ctx.mtm,
            text,
            frame.0,
            frame.1,
            frame.2,
            LabelStyle {
                size,
                color: token,
                height: frame.3,
                weight,
                wrap: false,
            },
            &ctx.palette,
        ));
    };
    add_label("Change models", frames.title, 15.0, "ink", Weight::Semibold);
    add_label(
        "Saved as soon as you pick. LocalFlow switches without a restart.",
        frames.detail,
        12.0,
        "muted",
        Weight::Regular,
    );
    add_label(
        "Speech model",
        frames.speech_label,
        12.0,
        "muted",
        Weight::Semibold,
    );
    add_label(
        CLEANUP_LABEL,
        frames.cleanup_label,
        12.0,
        "muted",
        Weight::Semibold,
    );
    add_label(
        &provider_status_text(&config),
        frames.provider_status,
        11.0,
        "muted",
        Weight::Regular,
    );
    {
        let weak = Rc::downgrade(state);
        let mtm = ctx.mtm;
        let services = ctx.services.clone();
        let rebuild = ctx.rebuild.clone();
        let import = button(
            mtm,
            "Import keys…",
            frames.import.0,
            frames.import.1,
            frames.import.2,
            frames.import.3,
            ButtonKind::Secondary,
            &ctx.palette,
        );
        import.setAccessibilityLabel(Some(&NSString::from_str("Import provider keys")));
        ctx.target.on_action(&import, move || {
            if let Some(state) = weak.upgrade() {
                import_keys(&state, &services, &rebuild, mtm);
            }
        });
        card.addSubview(&import);
    }
    {
        let backend = state
            .borrow()
            .backend
            .as_ref()
            .expect("page backend")
            .clone();
        let speech = speech_provider_choices(&config, &|id| backend.speech_available(id));
        let ids: Vec<String> = speech.iter().map(|(id, _)| id.clone()).collect();
        let titles: Vec<String> = speech.iter().map(|(_, title)| title.clone()).collect();
        let selected = ids.iter().position(|id| *id == provider);
        let weak = Rc::downgrade(state);
        let picker = popup(
            ctx.mtm,
            &titles,
            selected.unwrap_or(0),
            frames.speech_provider.0,
            frames.speech_provider.1,
            frames.speech_provider.2,
            "Speech engine",
        );
        picker.setFrame(rect_of(frames.speech_provider));
        fill_popup(&picker, &titles, selected);
        let picked_popup = picker.clone();
        ctx.target.on_action(&picker, move || {
            if let Some(state) = weak.upgrade() {
                let index = picked_popup.indexOfSelectedItem();
                if index < 0 {
                    return;
                }
                let id = state.borrow().quick.speech.get(index as usize).cloned();
                if let Some(id) = id {
                    quick_save_speech(&state, &id, None);
                }
            }
        });
        card.addSubview(&picker);
        state.borrow_mut().quick.speech = ids;
    }
    if let Some(frame) = frames.speech_fixed {
        add_label(
            "TDT 0.6B v2 (fixed model)",
            frame,
            13.0,
            "ink",
            Weight::Regular,
        );
    } else if let Some(frame) = frames.speech_model {
        let mut sizes = speech_model_choices(&provider)
            .into_iter()
            .map(|(id, label)| (id.to_string(), label.to_string()))
            .collect::<Vec<_>>();
        let current_size = config.field_str("model_size").unwrap_or("");
        if !sizes.iter().any(|(id, _)| id == current_size) {
            sizes.insert(
                0,
                (
                    current_size.to_string(),
                    format!("{current_size} (current)"),
                ),
            );
        }
        let ids: Vec<String> = sizes.iter().map(|(id, _)| id.clone()).collect();
        let titles: Vec<String> = sizes.iter().map(|(_, title)| title.clone()).collect();
        let selected = ids.iter().position(|id| id == current_size);
        let weak = Rc::downgrade(state);
        let picker = popup(
            ctx.mtm,
            &titles,
            selected.unwrap_or(0),
            frame.0,
            frame.1,
            frame.2,
            "Speech model size",
        );
        picker.setFrame(rect_of(frame));
        fill_popup(&picker, &titles, selected);
        let picked_popup = picker.clone();
        ctx.target.on_action(&picker, move || {
            if let Some(state) = weak.upgrade() {
                let index = picked_popup.indexOfSelectedItem();
                if index < 0 {
                    return;
                }
                let size = state.borrow().quick.sizes.get(index as usize).cloned();
                if let Some(size) = size {
                    let provider = current_speech_provider(
                        &state.borrow().services.as_ref().expect("services").config(),
                    );
                    quick_save_speech(&state, &provider, Some(&size));
                }
            }
        });
        card.addSubview(&picker);
        state.borrow_mut().quick.sizes = ids;
    }
    state.borrow_mut().quick.drawn_for = Some((
        config.field_str("llm_provider").unwrap_or("").to_string(),
        config.field_str("llm_base_url").unwrap_or("").to_string(),
    ));
    if ollama {
        let (items, selected, _) =
            cleanup_picker(&config, listing.unwrap_or(ListState::Loading), &models);
        let selected = items.iter().position(|item| *item == selected);
        let weak = Rc::downgrade(state);
        let picker = popup(
            ctx.mtm,
            &items,
            selected.unwrap_or(0),
            frames.cleanup_model.0,
            frames.cleanup_model.1,
            frames.cleanup_model.2,
            "Cleanup and Ask model",
        );
        picker.setFrame(rect_of(frames.cleanup_model));
        fill_popup(&picker, &items, selected);
        let picked_popup = picker.clone();
        ctx.target.on_action(&picker, move || {
            if let Some(state) = weak.upgrade() {
                let index = picked_popup.indexOfSelectedItem();
                if index < 0 {
                    return;
                }
                let model = state.borrow().quick.cleanup.get(index as usize).cloned();
                if let Some(model) = model {
                    let current = state.borrow().services.as_ref().expect("services").config();
                    if Some(model.as_str()) != current.field_str("llm_model") {
                        quick_save_cleanup(&state, model);
                    }
                }
            }
        });
        card.addSubview(&picker);
        if let Some(frame) = frames.refresh {
            let refresh = button(
                ctx.mtm,
                "Refresh",
                frame.0,
                frame.1,
                frame.2,
                frame.3,
                ButtonKind::Secondary,
                &ctx.palette,
            );
            refresh.setEnabled(listing != Some(ListState::Loading));
            let weak = Rc::downgrade(state);
            let rebuild = ctx.rebuild.clone();
            ctx.target.on_action(&refresh, move || {
                if let Some(state) = weak.upgrade() {
                    state.borrow_mut().quick.message = ModelsMessage::default();
                    let url = state
                        .borrow()
                        .services
                        .as_ref()
                        .and_then(|services| {
                            services
                                .config()
                                .field_str("llm_base_url")
                                .map(str::to_string)
                        })
                        .unwrap_or_default();
                    quick_fetch(&state, &url, true);
                    rebuild();
                }
            });
            card.addSubview(&refresh);
            state.borrow_mut().quick_controls.refresh = Some(refresh);
        }
        let mut state = state.borrow_mut();
        state.quick_controls.cleanup_model = Some(picker);
        state.quick.cleanup = items;
    } else {
        let value =
            draft.unwrap_or_else(|| config.field_str("llm_model").unwrap_or("").to_string());
        let cleanup = field(
            ctx.mtm,
            &value,
            frames.cleanup_model.0,
            frames.cleanup_model.1,
            frames.cleanup_model.2,
            "Cleanup and Ask model ID",
        );
        cleanup.setFrame(rect_of(frames.cleanup_model));
        cleanup.setPlaceholderString(Some(&NSString::from_str("The provider's model ID")));
        cleanup.setTextColor(Some(&color("ink", &ctx.palette)));
        cleanup.setBackgroundColor(Some(&color("paper", &ctx.palette)));
        cleanup.setEnabled(true);
        if let Some(cell) = cleanup.cell() {
            cell.setSendsActionOnEndEditing(true);
        }
        let weak = Rc::downgrade(state);
        let source = cleanup.clone();
        ctx.target.on_action(&cleanup, move || {
            if let Some(state) = weak.upgrade() {
                let typed = source.stringValue().to_string();
                let current = state.borrow().services.as_ref().expect("services").config();
                if typed.py_strip() == current.field_str("llm_model").unwrap_or("")
                    && state.borrow().quick.draft.is_none()
                {
                    return;
                }
                quick_save_cleanup(&state, typed);
            }
        });
        card.addSubview(&cleanup);
        state.borrow_mut().quick_controls.cleanup_field = Some(cleanup);
    }
    if let Some(frame) = frames.message {
        let note = label(
            ctx.mtm,
            &text,
            frame.0,
            frame.1,
            frame.2,
            LabelStyle {
                size: 12.0,
                color: if message.text.is_empty() {
                    "muted"
                } else if message.success {
                    "success"
                } else {
                    "red"
                },
                height: frame.3,
                wrap: true,
                ..LabelStyle::default()
            },
            &ctx.palette,
        );
        card.addSubview(&note);
        state.borrow_mut().quick_controls.note = Some(note);
    }
    state.borrow_mut().quick_geometry = Some(QuickGeometry {
        body: ctx.body.clone(),
        card,
        width: ctx.width,
        viewport_height: ctx.viewport_height,
        ollama,
        fixed,
    });
    top + frames.height + 12.0
}

// ------------------------------------------------------------- provider card ---

fn render_import(ctx: &PageCtx, state: &Shared, top: f64) -> f64 {
    let candidates = state.borrow().candidates.clone();
    if candidates.is_empty() {
        return top;
    }
    let height = import_height(candidates.len());
    let card = FlippedView::new(ctx.mtm, rect(0.0, top, ctx.width, height));
    style_surface(&card, &ctx.palette, &SurfaceStyle::default());
    ctx.body.addSubview(&card);
    let pad = f64::from(CARD_PADDING);
    card.addSubview(&label(
        ctx.mtm,
        "Keys found — select keys to import",
        pad,
        pad,
        ctx.width - 2.0 * pad,
        LabelStyle {
            size: 15.0,
            height: 22.0,
            weight: Weight::Semibold,
            ..LabelStyle::default()
        },
        &ctx.palette,
    ));
    card.addSubview(&label(
        ctx.mtm,
        "Only checked keys will be saved.",
        pad,
        pad + 24.0,
        ctx.width - 2.0 * pad,
        LabelStyle {
            size: 12.0,
            color: "muted",
            height: 18.0,
            ..LabelStyle::default()
        },
        &ctx.palette,
    ));
    let statuses = state.borrow().statuses.clone();
    let ticks = import_states(
        &candidates,
        &|id| statuses.get(id).is_some_and(|status| status.has_key),
        &state.borrow().checked,
    );
    let mut checks = Vec::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let title = format!(
            "{}: {} — {}",
            get_preset(&candidate.provider_id)
                .map(|preset| preset.label)
                .unwrap_or(candidate.provider_id.as_str()),
            candidate.masked_value(),
            candidate.source
        );
        // SAFETY: no target/action is needed; capture reads the checkbox state.
        let check = unsafe {
            NSButton::checkboxWithTitle_target_action(
                &NSString::from_str(&title),
                None,
                None,
                ctx.mtm,
            )
        };
        check.setFrame(rect(
            pad,
            pad + IMPORT_HEADER_H + index as f64 * IMPORT_ROW_H,
            ctx.width - 2.0 * pad,
            24.0,
        ));
        check.setContentTintColor(Some(&color("ink", &ctx.palette)));
        check.setAccessibilityLabel(Some(&NSString::from_str(&format!("Import {title}"))));
        check.setState(
            if ticks
                .get(&(candidate.provider_id.clone(), candidate.source.clone()))
                .copied()
                .unwrap_or(false)
            {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            },
        );
        card.addSubview(&check);
        checks.push(check);
    }
    state.borrow_mut().imports = checks;
    let save = button(
        ctx.mtm,
        "Save selected keys",
        pad,
        pad + IMPORT_HEADER_H + candidates.len() as f64 * IMPORT_ROW_H + 8.0,
        180.0,
        ACTION_H,
        ButtonKind::Secondary,
        &ctx.palette,
    );
    let weak = Rc::downgrade(state);
    let rebuild = ctx.rebuild.clone();
    ctx.target.on_action(&save, move || {
        if let Some(state) = weak.upgrade() {
            confirm_import(&state, &rebuild);
        }
    });
    card.addSubview(&save);
    top + height + 12.0
}

fn import_keys(
    state: &Shared,
    services: &Rc<HubServices>,
    rebuild: &Rc<dyn Fn()>,
    mtm: MainThreadMarker,
) {
    pull(state);
    let environ = if state.borrow().suspend_workers {
        BTreeMap::new()
    } else {
        std::env::vars().collect::<BTreeMap<_, _>>()
    };
    let mut found = detect_imports(None, &environ).unwrap_or_default();
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setTitle(Some(&NSString::from_str(
        "Import provider keys from a .env or shell profile (Cancel: environment only)",
    )));
    panel.setCanChooseDirectories(false);
    panel.setAllowsMultipleSelection(false);
    panel.setShowsHiddenFiles(true);
    let mut state = state.borrow_mut();
    state.checked.clear();
    if panel.runModal() == NSModalResponseOK {
        let path = panel
            .URL()
            .and_then(|url| url.path())
            .map(|path| path.to_string());
        match path
            .ok_or_else(|| anyhow!("chosen URL has no path"))
            .and_then(|path| detect_imports(Some(Path::new(&path)), &BTreeMap::new()))
        {
            Ok(mut file_keys) => found.append(&mut file_keys),
            Err(_) => {
                state.candidates = found;
                state.provider_message = ModelsMessage {
                    text: "Unable to read that file; environment results shown".to_string(),
                    success: false,
                };
                drop(state);
                rebuild();
                return;
            }
        }
    }
    let has_keys = !found.is_empty();
    state.candidates = found;
    state.provider_message = ModelsMessage {
        text: if has_keys {
            "Select the keys to save"
        } else {
            "No keys found"
        }
        .to_string(),
        success: has_keys,
    };
    let _ = services;
    drop(state);
    rebuild();
}

fn confirm_import(state: &Shared, rebuild: &Rc<dyn Fn()>) {
    let (selected, backend) = {
        let state = state.borrow();
        let selected = state
            .candidates
            .iter()
            .zip(&state.imports)
            .filter(|(_, check)| check.state() == NSControlStateValueOn)
            .map(|(candidate, _)| candidate.clone())
            .collect::<Vec<_>>();
        (
            selected,
            state.backend.as_ref().expect("page backend").clone(),
        )
    };
    let outcome = backend.save_imports(&selected);
    let mut state = state.borrow_mut();
    match outcome {
        Ok(()) => {
            state.candidates.clear();
            state.checked.clear();
            state.provider_message = ModelsMessage {
                text: "Selected keys saved".to_string(),
                success: true,
            };
        }
        Err(_) => {
            state.provider_message = ModelsMessage {
                text: IMPORT_SAVE_FAILED.to_string(),
                success: false,
            };
        }
    }
    drop(state);
    rebuild();
}

fn render_providers(ctx: &PageCtx, state: &Shared, top: f64) -> f64 {
    let config = ctx.services.config();
    let expanded_index = state
        .borrow()
        .expanded
        .as_ref()
        .and_then(|id| PRESETS.iter().position(|preset| preset.id == *id));
    state.borrow_mut().expanded = expanded_index.map(|index| PRESETS[index].id.to_string());
    let inner = ctx.width - 2.0 * f64::from(CARD_PADDING);
    let (editor_rows, editor_height) = if let Some(index) = expanded_index {
        let preset = &PRESETS[index];
        let message = state
            .borrow()
            .providers
            .get(preset.id)
            .map(|provider| provider.message.text.clone())
            .unwrap_or_default();
        editor_layout(
            inner,
            preset.id == "custom",
            preset.needs_key,
            true,
            message.chars().count().div_ceil(85),
        )
    } else {
        editor_layout(inner, false, false, false, 0)
    };
    let geometry = list_layout(
        ctx.width,
        PRESETS.len(),
        expanded_index,
        if expanded_index.is_some() {
            editor_height
        } else {
            0.0
        },
    );
    let card = FlippedView::new(ctx.mtm, rect(0.0, top, ctx.width, geometry.height));
    style_surface(&card, &ctx.palette, &SurfaceStyle::default());
    if let Some(layer) = card.layer() {
        layer.setMasksToBounds(true);
    }
    ctx.body.addSubview(&card);
    for (index, preset) in PRESETS.iter().enumerate() {
        let row_top = geometry.rows[index];
        {
            let mut state = state.borrow_mut();
            provider_state(&mut state, preset, &config);
        }
        let (provider, status) = {
            let state = state.borrow();
            (
                state
                    .providers
                    .get(preset.id)
                    .map(|provider| (provider.url.clone(), provider.model.clone())),
                state.statuses.get(preset.id).cloned(),
            )
        };
        let has_key = status.as_ref().is_some_and(|status| status.has_key);
        let (pill_text, (tint, text_token)) = if status.is_some() {
            provider_status(preset, &config, has_key)
        } else {
            ("Unknown", ("muted", "muted"))
        };
        let pill = make_pill(ctx.mtm, pill_text, tint, text_token, &ctx.palette);
        let pill_width = pill.frame().size.width;
        let columns = row_columns(ctx.width, pill_width);
        if index != 0 {
            let line = NSView::initWithFrame(
                ctx.mtm.alloc::<NSView>(),
                rect(
                    f64::from(CARD_PADDING),
                    row_top,
                    ctx.width - 2.0 * f64::from(CARD_PADDING),
                    1.0,
                ),
            );
            line.setWantsLayer(true);
            if let Some(layer) = line.layer() {
                layer.setBackgroundColor(Some(&color("line", &ctx.palette).CGColor()));
            }
            hide_from_accessibility(&line);
            card.addSubview(&line);
        }
        let (text_x, text_w) = columns.text;
        card.addSubview(&label(
            ctx.mtm,
            preset.label,
            text_x,
            row_top + 6.0,
            text_w,
            LabelStyle {
                size: 13.0,
                height: 18.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            &ctx.palette,
        ));
        let shown_url = if preset.id == "custom" {
            provider.map(|provider| provider.0).unwrap_or_default()
        } else {
            preset.base_url.to_string()
        };
        card.addSubview(&label(
            ctx.mtm,
            if shown_url.is_empty() {
                "Your inference endpoint"
            } else {
                &shown_url
            },
            text_x,
            row_top + 26.0,
            text_w,
            LabelStyle {
                size: 11.0,
                color: "muted",
                height: 16.0,
                ..LabelStyle::default()
            },
            &ctx.palette,
        ));
        pill.setFrameOrigin(NSPoint::new(
            columns.pill,
            row_top + (f64::from(MODELS_ROW_HEIGHT) - f64::from(PILL_HEIGHT)) / 2.0,
        ));
        card.addSubview(&pill);
        let configured = has_key || !preset.needs_key || is_active(preset, &config, None);
        let title = if expanded_index == Some(index) {
            "Close"
        } else if configured {
            "Edit"
        } else {
            "Set up"
        };
        let toggle = button(
            ctx.mtm,
            title,
            columns.button.0,
            row_top + 10.0,
            columns.button.1,
            ACTION_H,
            ButtonKind::Secondary,
            &ctx.palette,
        );
        toggle.setAccessibilityLabel(Some(&NSString::from_str(&format!(
            "{title}: {}",
            preset.label
        ))));
        let weak = Rc::downgrade(state);
        let id = preset.id.to_string();
        let rebuild = ctx.rebuild.clone();
        ctx.target.on_action(&toggle, move || {
            if let Some(state) = weak.upgrade() {
                pull(&state);
                state.borrow_mut().clear_controls();
                let mut current = state.borrow_mut();
                current.expanded = if current.expanded.as_deref() == Some(id.as_str()) {
                    None
                } else {
                    Some(id.clone())
                };
                drop(current);
                rebuild();
            }
        });
        card.addSubview(&toggle);
    }
    if let (Some(index), Some(editor_top)) = (expanded_index, geometry.editor_y) {
        render_editor(
            ctx,
            state,
            &card,
            &PRESETS[index],
            &editor_rows,
            editor_top,
            editor_height,
        );
    }
    top + geometry.height + 12.0
}

fn render_editor(
    ctx: &PageCtx,
    state: &Shared,
    card: &FlippedView,
    preset: &Preset,
    rows: &EditorRows,
    top: f64,
    height: f64,
) {
    let editor = FlippedView::new(ctx.mtm, rect(0.0, top, ctx.width, height));
    editor.setWantsLayer(true);
    if let Some(layer) = editor.layer() {
        layer.setBackgroundColor(Some(&color("paper", &ctx.palette).CGColor()));
    }
    card.addSubview(&editor);
    let caption = |frame: (f64, f64, f64, f64), text: &str| {
        editor.addSubview(&label(
            ctx.mtm,
            text,
            frame.0,
            frame.1,
            frame.2,
            LabelStyle {
                size: 11.0,
                color: "muted",
                height: frame.3,
                weight: Weight::Semibold,
                wrap: false,
            },
            &ctx.palette,
        ));
    };
    let (url_text, model_text, message, models) = {
        let state = state.borrow();
        let provider = state.providers.get(preset.id).expect("provider state");
        (
            provider.url.clone(),
            provider.model.clone(),
            provider.message.clone(),
            provider.models.clone(),
        )
    };
    let status = state.borrow().statuses.get(preset.id).cloned();
    let mut url_field = None;
    if let (Some(label_frame), Some(field_frame)) = (rows.url_label, rows.url_field) {
        caption(label_frame, "Base URL");
        let url = field(
            ctx.mtm,
            &url_text,
            field_frame.0,
            field_frame.1,
            field_frame.2,
            "Custom provider base URL",
        );
        url.setFrame(rect_of(field_frame));
        url.setPlaceholderString(Some(&NSString::from_str(
            "https://your-provider.example/v1",
        )));
        url.setTextColor(Some(&color("ink", &ctx.palette)));
        url.setBackgroundColor(Some(&color("paper", &ctx.palette)));
        editor.addSubview(&url);
        url_field = Some(url);
    }
    let mut secure = None;
    if let (Some(label_frame), Some(field_frame)) = (rows.key_label, rows.key_field) {
        caption(
            label_frame,
            &format!(
                "API key · {}",
                status
                    .as_ref()
                    .map(|status| if status.masked_value.is_empty() {
                        "none saved"
                    } else {
                        status.masked_value.as_str()
                    })
                    .unwrap_or("unavailable")
            ),
        );
        let key = NSSecureTextField::initWithFrame(
            ctx.mtm.alloc::<NSSecureTextField>(),
            rect_of(field_frame),
        );
        key.setPlaceholderString(Some(&NSString::from_str("Paste API key")));
        key.setAccessibilityLabel(Some(&NSString::from_str(&format!(
            "{} API key",
            preset.label
        ))));
        key.setTextColor(Some(&color("ink", &ctx.palette)));
        key.setBackgroundColor(Some(&color("paper", &ctx.palette)));
        editor.addSubview(&key);
        secure = Some(key);
        if let (Some(save_frame), Some(remove_frame)) = (rows.key_save, rows.key_remove) {
            for (title, remove, frame) in
                [("Save", false, save_frame), ("Remove", true, remove_frame)]
            {
                let action = button(
                    ctx.mtm,
                    title,
                    frame.0,
                    frame.1,
                    frame.2,
                    frame.3,
                    ButtonKind::Secondary,
                    &ctx.palette,
                );
                action.setAccessibilityLabel(Some(&NSString::from_str(&format!(
                    "{title} {} API key",
                    preset.label
                ))));
                if remove {
                    action.setEnabled(status.as_ref().is_some_and(|status| status.has_key));
                }
                let weak = Rc::downgrade(state);
                let id = preset.id.to_string();
                let rebuild = ctx.rebuild.clone();
                ctx.target.on_action(&action, move || {
                    if let Some(state) = weak.upgrade() {
                        provider_action(&state, &id, if remove { "remove" } else { "save" });
                        rebuild();
                    }
                });
                editor.addSubview(&action);
            }
        }
    }
    caption(rows.model_label, "Model");
    let combo = NSComboBox::initWithFrame(ctx.mtm.alloc::<NSComboBox>(), rect_of(rows.model_field));
    combo.setEditable(true);
    combo.setCompletes(false);
    combo.setAccessibilityLabel(Some(&NSString::from_str(&format!(
        "{} model ID",
        preset.label
    ))));
    // SAFETY: all object values are NSString instances.
    unsafe { combo.addItemsWithObjectValues(&combo_items(&models)) };
    combo.setStringValue(&NSString::from_str(&model_text));
    combo.setPlaceholderString(Some(&NSString::from_str("Choose or enter a model ID")));
    combo.setTextColor(Some(&color("ink", &ctx.palette)));
    combo.setBackgroundColor(Some(&color("paper", &ctx.palette)));
    editor.addSubview(&combo);
    if let Some(frame) = rows.use_button {
        let use_button = button(
            ctx.mtm,
            "Use for cleanup & Ask",
            frame.0,
            frame.1,
            frame.2,
            frame.3,
            ButtonKind::Primary,
            &ctx.palette,
        );
        use_button.setAccessibilityLabel(Some(&NSString::from_str(&format!(
            "Use {} for cleanup and Ask",
            preset.label
        ))));
        use_button
            .setEnabled(status.as_ref().is_some_and(|status| status.has_key) || !preset.needs_key);
        let weak = Rc::downgrade(state);
        let id = preset.id.to_string();
        let rebuild = ctx.rebuild.clone();
        ctx.target.on_action(&use_button, move || {
            if let Some(state) = weak.upgrade() {
                provider_action(&state, &id, "activate");
                rebuild();
            }
        });
        editor.addSubview(&use_button);
        let test = button(
            ctx.mtm,
            "Test connection",
            rows.test.0,
            rows.test.1,
            rows.test.2,
            rows.test.3,
            ButtonKind::Secondary,
            &ctx.palette,
        );
        test.setAccessibilityLabel(Some(&NSString::from_str(&format!(
            "Test {} connection",
            preset.label
        ))));
        let weak = Rc::downgrade(state);
        let id = preset.id.to_string();
        let rebuild = ctx.rebuild.clone();
        ctx.target.on_action(&test, move || {
            if let Some(state) = weak.upgrade() {
                pull(&state);
                let mut current = state.borrow_mut();
                if let Some(provider) = current.providers.get_mut(&id) {
                    provider.message = ModelsMessage {
                        text: "Testing connection…".to_string(),
                        success: true,
                    };
                }
                drop(current);
                start_provider_test(&state, &id);
                rebuild();
            }
        });
        editor.addSubview(&test);
        state.borrow_mut().editor = Some(EditorControls {
            provider_id: preset.id.to_string(),
            url: url_field.clone(),
            combo: combo.clone(),
            secure,
            use_button,
            test,
            rows: rows.clone(),
        });
        apply_use(state);
    }
    if let Some(source) = &url_field {
        let weak = Rc::downgrade(state);
        let url = source.clone();
        ctx.target.on_field_change(source, move || {
            if let Some(state) = weak.upgrade() {
                let value = url.stringValue().to_string();
                let id = {
                    let mut state = state.borrow_mut();
                    let Some(editor) = &state.editor else { return };
                    let id = editor.provider_id.clone();
                    if let Some(provider) = state.providers.get_mut(&id) {
                        provider.url = value.py_strip().to_string();
                    }
                    id
                };
                let _ = id;
                apply_use(&state);
            }
        });
    }
    let weak = Rc::downgrade(state);
    let source = combo.clone();
    ctx.target.on_field_change(&combo, move || {
        if let Some(state) = weak.upgrade() {
            update_combo(&state, &source, None);
        }
    });
    let weak = Rc::downgrade(state);
    let source = combo.clone();
    ctx.target.on_selection_change(&combo, move || {
        if let Some(state) = weak.upgrade() {
            // During comboBoxSelectionDidChange the new item is not yet in
            // stringValue, so explicitly read objectValueOfSelectedItem.
            let picked = source
                .objectValueOfSelectedItem()
                .and_then(|value| value.downcast::<NSString>().ok())
                .map(|value| value.to_string());
            update_combo(&state, &source, picked);
        }
    });
    if let Some(frame) = rows.message {
        editor.addSubview(&label(
            ctx.mtm,
            &message.text,
            frame.0,
            frame.1,
            frame.2,
            LabelStyle {
                size: 12.0,
                color: if message.success { "success" } else { "red" },
                height: frame.3,
                wrap: true,
                ..LabelStyle::default()
            },
            &ctx.palette,
        ));
    }
    let cache_key = (
        url_text,
        status
            .as_ref()
            .map(|status| status.masked_value.clone())
            .unwrap_or_default(),
    );
    let should_fetch = {
        let mut state = state.borrow_mut();
        let provider = state.providers.get_mut(preset.id).expect("provider state");
        let fetch = provider.loaded.as_ref() != Some(&cache_key)
            && (status.as_ref().is_some_and(|status| status.has_key) || !preset.needs_key);
        if fetch {
            provider.loaded = Some(cache_key);
        }
        fetch
    };
    if should_fetch {
        start_provider_models(state, preset.id);
    }
}

fn update_combo(state: &Shared, combo: &NSComboBox, picked: Option<String>) {
    let value = picked.unwrap_or_else(|| combo.stringValue().to_string());
    {
        let mut state = state.borrow_mut();
        let Some(editor) = &state.editor else { return };
        let id = editor.provider_id.clone();
        if let Some(provider) = state.providers.get_mut(&id) {
            provider.model = value.py_strip().to_string();
        }
    }
    // Never read/clear the secure field, rebuild, or move focus on live edits.
    apply_use(state);
}

fn provider_action(state: &Shared, provider_id: &str, operation: &str) {
    pull(state);
    let (services, backend, url, model, secure) = {
        let state = state.borrow();
        let Some(services) = &state.services else {
            return;
        };
        let provider = state.providers.get(provider_id).expect("provider state");
        (
            services.clone(),
            state.backend.as_ref().expect("page backend").clone(),
            provider.url.clone(),
            provider.model.clone(),
            state
                .editor
                .as_ref()
                .filter(|editor| editor.provider_id == provider_id)
                .and_then(|editor| editor.secure.clone()),
        )
    };
    let result = match operation {
        "save" => {
            let Some(secure) = secure else {
                return;
            };
            // The secret is read only at the action boundary. It is cleared on
            // both success and failure, and backend errors are never shown raw.
            let value = secure.stringValue();
            let result = backend.save_key(provider_id, &value.to_string());
            secure.setStringValue(&NSString::from_str(""));
            result.map(|_| None)
        }
        "remove" => backend
            .remove_key(provider_id, &services.config())
            .map(|_| None),
        "activate" => match fresh_config(&services) {
            Ok(config) => backend
                .activate(provider_id, &url, &model, &config, &|updated| {
                    persist_config(&services, &config, updated)
                })
                .map(Some),
            Err(_) => Err(anyhow!("config could not be loaded")),
        },
        _ => return,
    };
    let mut state = state.borrow_mut();
    match result {
        Ok(updated) => {
            if let Some(updated) = updated {
                commit_config(&services, updated);
            }
            if let Some(provider) = state.providers.get_mut(provider_id) {
                provider.message = ModelsMessage {
                    text: "Provider settings saved".to_string(),
                    success: true,
                };
            }
        }
        Err(_) => {
            if let Some(provider) = state.providers.get_mut(provider_id) {
                provider.message = ModelsMessage {
                    text: SAVE_FAILED.to_string(),
                    success: false,
                };
            }
        }
    }
}

// --------------------------------------------------------------- storage card ---

fn render_storage(ctx: &PageCtx, state: &Shared, top: f64) -> f64 {
    let config = ctx.services.config();
    let (models, error) = {
        let state = state.borrow();
        if let Some(models) = &state.seeded_storage {
            (models.clone(), None)
        } else {
            let backend = state.backend.as_ref().expect("page backend").clone();
            match backend.inventory(&config) {
                Ok(models) => (models, None),
                Err(_) => (Vec::new(), Some(STORAGE_READ_FAILED.to_string())),
            }
        }
    };
    if let Some(error) = error {
        state.borrow_mut().storage_message = ModelsMessage {
            text: error,
            success: false,
        };
    }
    let message = state.borrow().storage_message.clone();
    let rows_h = models.len().max(1) as f64 * ROW_H;
    let height = STORAGE_TOP
        + rows_h
        + if message.text.is_empty() { 0.0 } else { 26.0 }
        + f64::from(CARD_PADDING);
    let card = FlippedView::new(ctx.mtm, rect(0.0, top, ctx.width, height));
    style_surface(
        &card,
        &ctx.palette,
        &SurfaceStyle {
            border: "ink",
            width: Some(f64::from(CARD_BORDER_WIDTH)),
            ..SurfaceStyle::default()
        },
    );
    ctx.body.addSubview(&card);
    let pad = f64::from(CARD_PADDING);
    card.addSubview(&label(
        ctx.mtm,
        "Free up space",
        pad,
        pad,
        ctx.width - 2.0 * pad,
        LabelStyle {
            size: 15.0,
            height: 22.0,
            weight: Weight::Semibold,
            ..LabelStyle::default()
        },
        &ctx.palette,
    ));
    card.addSubview(&label(
        ctx.mtm,
        "Downloaded speech models. Models in use or selected are kept; others can be \
         downloaded again any time.",
        pad,
        pad + 24.0,
        ctx.width - 2.0 * pad,
        LabelStyle {
            size: 12.0,
            color: "muted",
            height: 18.0,
            ..LabelStyle::default()
        },
        &ctx.palette,
    ));
    let inner = ctx.width - 2.0 * pad;
    if models.is_empty() {
        card.addSubview(&label(
            ctx.mtm,
            "No other speech models are taking up space.",
            pad,
            STORAGE_TOP + 6.0,
            inner,
            LabelStyle {
                size: 13.0,
                color: "muted",
                height: 22.0,
                ..LabelStyle::default()
            },
            &ctx.palette,
        ));
    }
    for (index, model) in models.iter().enumerate() {
        let y = STORAGE_TOP + index as f64 * ROW_H;
        card.addSubview(&label(
            ctx.mtm,
            &model.label,
            pad,
            y + 6.0,
            inner - 260.0,
            LabelStyle {
                size: 13.0,
                height: 22.0,
                weight: Weight::Semibold,
                ..LabelStyle::default()
            },
            &ctx.palette,
        ));
        card.addSubview(&label(
            ctx.mtm,
            &format_bytes(model.size_bytes),
            pad + inner - 250.0,
            y + 7.0,
            100.0,
            LabelStyle {
                size: 12.0,
                color: "muted",
                height: 20.0,
                ..LabelStyle::default()
            },
            &ctx.palette,
        ));
        if model.protected() {
            let (tint, token) = if model.status == "In use" {
                ("success", "success")
            } else {
                ("line", "ink")
            };
            let pill = make_pill(ctx.mtm, &model.status, tint, token, &ctx.palette);
            let size = pill.frame().size;
            pill.setFrameOrigin(NSPoint::new(
                pad + inner - size.width,
                y + (ROW_H - size.height) / 2.0,
            ));
            card.addSubview(&pill);
        } else {
            let delete = button(
                ctx.mtm,
                "Delete…",
                pad + inner - 120.0,
                y + 2.0,
                120.0,
                32.0,
                ButtonKind::DestructiveGhost,
                &ctx.palette,
            );
            delete.setAccessibilityLabel(Some(&NSString::from_str(&format!(
                "Delete {}",
                model.label
            ))));
            let weak = Rc::downgrade(state);
            let rebuild = ctx.rebuild.clone();
            let services = ctx.services.clone();
            let mtm = ctx.mtm;
            let repo = model.repo.clone();
            let model_label = model.label.clone();
            let listed_size = model.size_bytes;
            ctx.target.on_action(&delete, move || {
                if let Some(state) = weak.upgrade() {
                    delete_storage_model(
                        &state,
                        &services,
                        &rebuild,
                        mtm,
                        &repo,
                        &model_label,
                        listed_size,
                    );
                }
            });
            card.addSubview(&delete);
        }
    }
    if !message.text.is_empty() {
        card.addSubview(&label(
            ctx.mtm,
            &message.text,
            pad,
            STORAGE_TOP + rows_h + 4.0,
            inner,
            LabelStyle {
                size: 12.0,
                color: if message.success { "success" } else { "red" },
                height: 20.0,
                ..LabelStyle::default()
            },
            &ctx.palette,
        ));
    }
    state.borrow_mut().storage = models;
    top + height + 12.0
}

fn delete_storage_model(
    state: &Shared,
    services: &Rc<HubServices>,
    rebuild: &Rc<dyn Fn()>,
    mtm: MainThreadMarker,
    repo: &str,
    model_label: &str,
    listed_size: u64,
) {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(&format!("Delete {model_label}?")));
    alert.setInformativeText(&NSString::from_str(&format!(
        "This frees {}. LocalFlow can download it again later.",
        format_bytes(listed_size)
    )));
    alert.addButtonWithTitle(&NSString::from_str("Delete"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    if alert.runModal() != NSAlertFirstButtonReturn {
        return;
    }
    let backend = state
        .borrow()
        .backend
        .as_ref()
        .expect("page backend")
        .clone();
    // Re-read settings at click time. The real backend also re-reads leases and
    // rechecks the catalog/symlink guard immediately before deleting.
    let outcome = fresh_config(services).and_then(|config| backend.delete_model(repo, &config));
    state.borrow_mut().storage_message = match outcome {
        Ok(freed) => ModelsMessage {
            text: format!("Deleted {model_label} and freed {}.", format_bytes(freed)),
            success: true,
        },
        Err(error) => ModelsMessage {
            text: error.to_string(),
            success: false,
        },
    };
    rebuild();
}

// --------------------------------------------------------------- Hub contract ---

impl HubPage for ModelsPage {
    fn prepare_snapshot(&mut self, preparation: &str, root: &std::path::Path) -> Result<()> {
        let mut seed = ModelsSnapshot::default();
        let keys = preparation.starts_with("keys-");
        let expanded = match preparation {
            "keys-expanded-active" => Some("fireworks"),
            "keys-expanded-inactive" => Some("openai"),
            "keys-expanded-no-key" => Some("groq"),
            "keys-expanded-custom" => Some("custom"),
            "keys-test-success" | "keys-test-failure" | "models-error" => Some("ollama"),
            _ => None,
        };
        seed.expanded = expanded.map(str::to_string);
        match preparation {
            "models-empty"
            | "models-error"
            | "keys-no-keys"
            | "keys-import-defaults"
            | "keys-expanded-no-key"
            | "change-loading"
            | "change-unavailable"
            | "change-empty" => {}
            "models-populated"
            | "models-parakeet"
            | "keys-saved-active"
            | "keys-import-results"
            | "keys-test-success"
            | "keys-test-failure"
            | "keys-expanded-active"
            | "keys-expanded-inactive"
            | "keys-expanded-custom"
            | "change-ollama"
            | "change-missing-model"
            | "change-cloud"
            | "change-invalid-id"
            | "storage"
            | "storage-deleted" => {}
            other => anyhow::bail!("unknown Models snapshot preparation {other:?}"),
        }
        let saved = matches!(
            preparation,
            "keys-saved-active"
                | "keys-import-results"
                | "keys-test-success"
                | "keys-test-failure"
                | "keys-expanded-active"
                | "keys-expanded-inactive"
        );
        // Python's general, change-model and error snapshots have no saved
        // keys. Provider snapshots save exactly the two fake import candidates
        // below, not a key for every provider. Include an explicit status for
        // every preset, as provider_keys.key_status does in the Python page.
        for preset in PRESETS {
            let has_key = saved && matches!(preset.id, "fireworks" | "openai");
            seed.statuses.insert(
                preset.id.into(),
                KeyStatus {
                    has_key,
                    masked_value: match (has_key, preset.id) {
                        (true, "fireworks") => "••••1234".into(),
                        (true, "openai") => "••••5678".into(),
                        _ => String::new(),
                    },
                },
            );
        }
        seed.providers = PRESETS
            .iter()
            .map(|preset| ProviderSnapshot {
                id: preset.id.into(),
                url: if preparation == "keys-expanded-custom" && preset.id == "custom" {
                    "https://llm.example.com/v1".into()
                } else {
                    preset.base_url.into()
                },
                model: if saved && preset.id == "fireworks" {
                    "example-model".into()
                } else {
                    preset.default_model.into()
                },
                models: if preparation == "models-empty" {
                    Vec::new()
                } else {
                    vec!["example-model".into()]
                },
                message: match (preparation, preset.id) {
                    ("keys-test-success", "ollama") => ModelsMessage {
                        text: lf_providers::keys::TEST_OK.into(),
                        success: true,
                    },
                    ("keys-test-failure", "ollama") => ModelsMessage {
                        text: lf_providers::keys::TEST_FAILED.into(),
                        success: false,
                    },
                    ("models-error", "ollama") => ModelsMessage {
                        text: "Unable to read model settings; using safe fixtures.".into(),
                        success: false,
                    },
                    _ => ModelsMessage::default(),
                },
            })
            .collect();
        // provider_snapshot_mode returns after putting Ollama into its initial
        // loading state. That quick state also survives the intervening cloud
        // configurations throughout Python's cumulative provider sequence.
        seed.quick_list_state = Some(if keys {
            ListState::Loading
        } else {
            match preparation {
                "change-loading" => ListState::Loading,
                "change-unavailable" => ListState::Unavailable,
                "change-empty" | "models-empty" => ListState::Empty,
                _ => ListState::Ready,
            }
        });
        seed.quick_models = match preparation {
            "change-ollama" => vec!["gemma4:e4b".into(), "llama3.2:3b".into(), "qwen3:4b".into()],
            "change-missing-model" => vec!["gemma4:e4b".into()],
            "models-populated" => {
                vec!["gemma4:e4b".into(), "llama3.2:3b".into(), "qwen3:4b".into()]
            }
            "models-parakeet" | "models-error" | "storage" | "storage-deleted" => {
                vec!["gemma4:e4b".into(), "llama3.2:3b".into()]
            }
            _ => Vec::new(),
        };
        if preparation == "change-invalid-id" {
            seed.quick_draft = Some("gpt 4o mini!".into());
            seed.quick_message = ModelsMessage {
                text: "Use the provider's model ID: letters, numbers and . _ - : / @ + only."
                    .into(),
                success: false,
            };
        }
        if matches!(preparation, "keys-import-results" | "keys-import-defaults") {
            seed.provider_message = ModelsMessage {
                text: "Select the keys to save".into(),
                success: true,
            };
        }
        let import_candidates =
            if matches!(preparation, "keys-import-results" | "keys-import-defaults") {
                let mut environment = BTreeMap::new();
                environment.insert(
                    "FIREWORKS_API_KEY".into(),
                    "snapshot-not-a-real-key-1234".into(),
                );
                environment.insert(
                    "OPENAI_API_KEY".into(),
                    "snapshot-not-a-real-key-5678".into(),
                );
                let mut candidates = lf_providers::keys::detect_imports(None, &environment)?;
                // The Python fixture supplies literal candidates rather than
                // detecting them: OpenAI came from this shell profile, not the
                // process environment used to create the private fake value.
                for candidate in &mut candidates {
                    if candidate.provider_id == "openai" {
                        candidate.source = "~/.zshrc: OPENAI_API_KEY".into();
                    }
                }
                Some(candidates)
            } else {
                None
            };
        if matches!(preparation, "storage" | "storage-deleted") {
            seed.storage = vec![
                CachedModel::new(
                    "mlx-community/parakeet-tdt-0.6b-v2",
                    "Parakeet v2",
                    2_472_000_000,
                    "",
                ),
                CachedModel::new(
                    "Systran/faster-whisper-small",
                    "Whisper Small",
                    486_000_000,
                    "In use",
                ),
                CachedModel::new(
                    "Systran/faster-whisper-base",
                    "Whisper Base",
                    148_000_000,
                    "Selected",
                ),
            ];
        }
        if preparation == "storage-deleted" {
            seed.storage.remove(0);
            seed.storage_message = ModelsMessage {
                text: "Deleted Parakeet v2 and freed 2.5 GB.".into(),
                success: true,
            };
        }
        {
            let state = self.state.borrow();
            if let Some(services) = &state.services {
                // Python creates a fresh default provider config for every
                // specification, instead of carrying earlier LLM or speech
                // selections into the next fixture.
                let mut config = if keys {
                    Config::default()
                } else {
                    services.config()
                };
                if saved {
                    config.set_field("llm_provider", serde_json::json!("openai-compatible"));
                    config.set_field(
                        "llm_base_url",
                        serde_json::json!("https://api.fireworks.ai/inference/v1"),
                    );
                    config.set_field("llm_model", serde_json::json!("example-model"));
                }
                if preparation == "models-parakeet" {
                    config.set_field("stt_backend", serde_json::json!("parakeet"));
                    config.set_field("model_size", serde_json::json!("large-v3"));
                    config.set_field("stt_device", serde_json::json!("cpu"));
                }
                if preparation == "change-cloud" {
                    config.set_field("llm_provider", serde_json::json!("openai-compatible"));
                    config.set_field(
                        "llm_base_url",
                        serde_json::json!("https://api.fireworks.ai/inference/v1"),
                    );
                    config.set_field(
                        "llm_model",
                        serde_json::json!("accounts/fireworks/models/example-model"),
                    );
                    config.set_field("stt_backend", serde_json::json!("parakeet"));
                }
                if matches!(preparation, "change-ollama" | "change-loading") {
                    config.set_field("llm_model", serde_json::json!("gemma4:e4b"));
                }
                if preparation == "change-missing-model" {
                    config.set_field("llm_model", serde_json::json!("llama3.2:3b"));
                }
                if preparation == "change-invalid-id" {
                    config.set_field("llm_provider", serde_json::json!("openai-compatible"));
                    config.set_field(
                        "llm_base_url",
                        serde_json::json!("https://api.openai.com/v1"),
                    );
                    config.set_field("llm_model", serde_json::json!("example-model"));
                }
                if matches!(preparation, "storage" | "storage-deleted") {
                    config.set_field("stt_backend", serde_json::json!("faster-whisper"));
                    config.set_field("model_size", serde_json::json!("base"));
                }
                *services.config.borrow_mut() = config;
            }
        }
        let _ = root;
        self.seed_snapshot(seed);
        if let Some(candidates) = import_candidates {
            self.seed_import_candidates(candidates);
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        PAGE
    }

    fn capture(&mut self) {
        pull(&self.state);
    }

    fn flush(&mut self) {
        // Called when leaving Models, not for same-page rebuilds.
        let mut state = self.state.borrow_mut();
        state.visible = false;
        state.clear_controls();
        state.services = None;
        state.rebuild = None;
    }

    fn release_views(&mut self) {
        let mut state = self.state.borrow_mut();
        state.visible = false;
        state.clear_controls();
        state.rebuild = None;
    }

    fn shutdown(&mut self) {
        let mut state = self.state.borrow_mut();
        state.closed = true;
        state.poll_timer = None;
        state.visible = false;
        state.quick.token = state.quick.token.wrapping_add(1);
        for provider in state.providers.values_mut() {
            provider.models_token = provider.models_token.wrapping_add(1);
            provider.test_token = provider.test_token.wrapping_add(1);
        }
        state.clear_controls();
        state.services = None;
        state.rebuild = None;
    }

    fn render(&mut self, ctx: &PageCtx, top: f64) -> f64 {
        if ctx.services.memory_only_config.get() && self.state.borrow().system_backend {
            let mut state = self.state.borrow_mut();
            state.backend = Some(Arc::new(NoExternalBackend));
            state.suspend_workers = true;
            state.system_backend = false;
        }
        let config = match fresh_config(&ctx.services) {
            Ok(config) => {
                commit_config(&ctx.services, config.clone());
                config
            }
            Err(_) => {
                self.state.borrow_mut().provider_message = ModelsMessage {
                    text: CONFIG_LOAD_FAILED.to_string(),
                    success: false,
                };
                ctx.services.config()
            }
        };
        ensure_poll_timer(&self.state);
        self.state.borrow_mut().clear_controls();
        {
            let mut state = self.state.borrow_mut();
            state.visible = true;
            state.closed = false;
            state.services = Some(ctx.services.clone());
            state.rebuild = Some(ctx.rebuild.clone());
            state.palette = Some(ctx.palette);
            state.measure = Some(ctx.measure.clone());
            state.mtm = Some(ctx.mtm);
            state.statuses = if let Some(statuses) = &state.seeded_statuses {
                statuses.clone()
            } else {
                let backend = state.backend.as_ref().expect("page backend").clone();
                let mut statuses = BTreeMap::new();
                let mut failed = false;
                for preset in PRESETS {
                    match backend.key_status(preset.id) {
                        Ok(status) => {
                            statuses.insert(preset.id.to_string(), status);
                        }
                        Err(_) => failed = true,
                    }
                }
                if failed {
                    state.provider_message = ModelsMessage {
                        text: "Unable to read saved keys; check Keychain access".to_string(),
                        success: false,
                    };
                }
                statuses
            };
            for preset in PRESETS {
                provider_state(&mut state, &preset, &config);
            }
        }
        let mut cursor = render_quick(ctx, &self.state, top);
        let provider_message = self.state.borrow().provider_message.clone();
        if !provider_message.text.is_empty() {
            ctx.body.addSubview(&label(
                ctx.mtm,
                &provider_message.text,
                0.0,
                cursor,
                ctx.width,
                LabelStyle {
                    size: 14.0,
                    color: if provider_message.success {
                        "success"
                    } else {
                        "red"
                    },
                    height: 20.0,
                    ..LabelStyle::default()
                },
                &ctx.palette,
            ));
            cursor += 28.0;
        }
        cursor = render_import(ctx, &self.state, cursor);
        cursor = render_providers(ctx, &self.state, cursor);
        render_storage(ctx, &self.state, cursor)
    }
}

// ---------------------------------------------------------- real local storage ---

const PARAKEET_REPO: &str = "mlx-community/parakeet-tdt-0.6b-v2";
const FW_REPOS: [(&str, &str); 7] = [
    ("tiny", "Systran/faster-whisper-tiny"),
    ("base", "Systran/faster-whisper-base"),
    ("small", "Systran/faster-whisper-small"),
    ("medium", "Systran/faster-whisper-medium"),
    (
        "large-v3-turbo",
        "mobiuslabsgmbh/faster-whisper-large-v3-turbo",
    ),
    ("large-v3", "Systran/faster-whisper-large-v3"),
    ("distil-large-v3", "Systran/faster-distil-whisper-large-v3"),
];
const MLX_REPOS: [(&str, &str); 6] = [
    ("tiny", "mlx-community/whisper-tiny-mlx"),
    ("base", "mlx-community/whisper-base-mlx"),
    ("small", "mlx-community/whisper-small-mlx"),
    ("medium", "mlx-community/whisper-medium-mlx"),
    ("large-v3", "mlx-community/whisper-large-v3-mlx"),
    ("large-v3-turbo", "mlx-community/whisper-large-v3-turbo"),
];

fn size_label(size: &str) -> &str {
    match size {
        "tiny" => "Tiny",
        "base" => "Base",
        "small" => "Small",
        "medium" => "Medium",
        "large-v3" => "Large v3",
        "large-v3-turbo" => "Large v3 Turbo",
        "distil-large-v3" => "Distil Large v3",
        _ => size,
    }
}

fn catalog() -> Vec<(String, String)> {
    let mut repos = FW_REPOS
        .iter()
        .map(|(size, repo)| (repo.to_string(), format!("Whisper {}", size_label(size))))
        .collect::<Vec<_>>();
    repos.extend(MLX_REPOS.iter().map(|(size, repo)| {
        (
            repo.to_string(),
            format!("Whisper {} (Apple Silicon)", size_label(size)),
        )
    }));
    repos.push((PARAKEET_REPO.to_string(), "Parakeet v2".to_string()));
    repos
}

/// Same size formatting as `model_storage.format_bytes` (decimal units).
pub fn format_bytes(size: u64) -> String {
    let mut value = size as f64;
    for unit in ["bytes", "KB", "MB", "GB"] {
        if value < 1000.0 || unit == "GB" {
            return if unit == "bytes" {
                format!("{} bytes", size)
            } else if value < 10.0 {
                format!("{value:.1} {unit}")
            } else {
                format!("{value:.0} {unit}")
            };
        }
        value /= 1000.0;
    }
    unreachable!()
}

fn cache_root(override_path: Option<&Path>) -> PathBuf {
    if let Some(path) = override_path {
        return path.to_path_buf();
    }
    if let Some(path) = std::env::var_os("HF_HUB_CACHE") {
        return expand_home(&PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("HUGGINGFACE_HUB_CACHE") {
        return expand_home(&PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("HF_HOME") {
        return expand_home(&PathBuf::from(path)).join("hub");
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            lf_core::home::python_home()
                .unwrap_or_default()
                .join(".cache")
        });
    expand_home(&base).join("huggingface").join("hub")
}

fn expand_home(path: &Path) -> PathBuf {
    lf_core::home::expand_user(path).unwrap_or_else(|_| path.to_path_buf())
}

fn repo_folder_name(repo: &str) -> String {
    format!("models--{}", repo.replace('/', "--"))
}

fn model_folder(cache: &Path, repo: &str) -> Option<PathBuf> {
    let folder = cache.join(repo_folder_name(repo));
    let link = std::fs::symlink_metadata(&folder).ok()?;
    if link.file_type().is_symlink() || !link.is_dir() {
        return None;
    }
    let canonical = folder.canonicalize().ok()?;
    let parent = canonical.parent()?;
    if parent != cache.canonicalize().ok()? {
        return None;
    }
    Some(folder)
}

#[derive(Debug)]
struct Protection {
    running: BTreeSet<String>,
    selected: BTreeSet<String>,
    unknown: String,
}

fn faster_whisper_repo(size: &str) -> Option<String> {
    let known = match size {
        "tiny.en" => "Systran/faster-whisper-tiny.en",
        "tiny" => "Systran/faster-whisper-tiny",
        "base.en" => "Systran/faster-whisper-base.en",
        "base" => "Systran/faster-whisper-base",
        "small.en" => "Systran/faster-whisper-small.en",
        "small" => "Systran/faster-whisper-small",
        "medium.en" => "Systran/faster-whisper-medium.en",
        "medium" => "Systran/faster-whisper-medium",
        "large-v1" => "Systran/faster-whisper-large-v1",
        "large-v2" => "Systran/faster-whisper-large-v2",
        "large-v3" | "large" => "Systran/faster-whisper-large-v3",
        "distil-large-v2" => "Systran/faster-distil-whisper-large-v2",
        "distil-medium.en" => "Systran/faster-distil-whisper-medium.en",
        "distil-small.en" => "Systran/faster-distil-whisper-small.en",
        "distil-large-v3" => "Systran/faster-distil-whisper-large-v3",
        "distil-large-v3.5" => "distil-whisper/distil-large-v3.5-ct2",
        "large-v3-turbo" | "turbo" => "mobiuslabsgmbh/faster-whisper-large-v3-turbo",
        _ if size.split('/').count() == 2 && !size.starts_with(['/', '.', '~']) => size,
        _ => return None,
    };
    Some(known.to_string())
}

fn selected_repos(
    config: &Config,
    mlx_available: bool,
) -> std::result::Result<BTreeSet<String>, String> {
    let backend = config.field_str("stt_backend").unwrap_or("");
    let size = config.field_str("model_size").unwrap_or("");
    let device = config.field_str("stt_device").unwrap_or("");
    if backend == "parakeet" {
        return Ok(BTreeSet::from([PARAKEET_REPO.to_string()]));
    }
    if !matches!(backend, "auto" | "mlx" | "mlx-whisper" | "faster-whisper") {
        return Err("Your speech provider is a plug-in, so LocalFlow can't tell which model it uses. Nothing was deleted.".to_string());
    }
    let mut repos = BTreeSet::new();
    if matches!(backend, "mlx" | "mlx-whisper")
        || (backend == "auto" && device == "auto" && mlx_available)
    {
        if let Some((_, repo)) = MLX_REPOS.iter().find(|(candidate, _)| *candidate == size) {
            repos.insert((*repo).to_string());
        }
    }
    if let Some(repo) = faster_whisper_repo(size) {
        repos.insert(repo);
    }
    Ok(repos)
}

fn protection(config: &Config, leases: Option<&Path>, mlx_available: bool) -> Protection {
    let (running, mut unknown) = live_leases(leases);
    let selected = match selected_repos(config, mlx_available) {
        Ok(selected) => selected,
        Err(reason) => {
            if unknown.is_empty() {
                unknown = reason;
            }
            BTreeSet::new()
        }
    };
    Protection {
        running,
        selected,
        unknown,
    }
}

fn storage_inventory(
    config: &Config,
    cache: Option<&Path>,
    leases: Option<&Path>,
    mlx_available: bool,
) -> Result<Vec<CachedModel>> {
    let cache = cache_root(cache);
    let guard = protection(config, leases, mlx_available);
    let mut found = Vec::new();
    for (repo, label) in catalog() {
        let Some(folder) = model_folder(&cache, &repo) else {
            continue;
        };
        let size_bytes = folder_size(&folder);
        if size_bytes == 0 {
            continue;
        }
        let status = if guard.running.contains(&repo) {
            "In use".to_string()
        } else if guard.selected.contains(&repo) {
            "Selected".to_string()
        } else if !guard.unknown.is_empty() {
            "Unknown use".to_string()
        } else {
            String::new()
        };
        found.push(CachedModel {
            repo,
            label,
            size_bytes,
            status,
        });
    }
    found.sort_by(|left, right| {
        right
            .size_bytes
            .cmp(&left.size_bytes)
            .then_with(|| left.label.cmp(&right.label))
    });
    Ok(found)
}

fn storage_delete(
    repo: &str,
    config: &Config,
    cache: Option<&Path>,
    leases: Option<&Path>,
    mlx_available: bool,
) -> Result<u64> {
    let Some(label) = catalog()
        .into_iter()
        .find(|(candidate, _)| candidate == repo)
        .map(|(_, label)| label)
    else {
        return Err(anyhow!("That isn't a speech model LocalFlow manages."));
    };
    let guard = protection(config, leases, mlx_available);
    if !guard.unknown.is_empty() {
        return Err(anyhow!(guard.unknown));
    }
    if guard.running.contains(repo) {
        return Err(anyhow!(
            "{label} is loaded by a running LocalFlow process, so it was kept."
        ));
    }
    if guard.selected.contains(repo) {
        return Err(anyhow!(
            "{label} is the speech model in your settings, so it was kept."
        ));
    }
    let cache = cache_root(cache);
    let Some(folder) = model_folder(&cache, repo) else {
        return Err(anyhow!(
            "{label} isn't downloaded (or isn't a plain folder)."
        ));
    };
    let before = folder_size(&folder);
    match std::fs::remove_dir_all(&folder) {
        Ok(()) => Ok(before),
        Err(error) => {
            let left = if folder.exists() {
                folder_size(&folder)
            } else {
                0
            };
            let freed = before.saturating_sub(left);
            Err(anyhow!(
                "Couldn't fully delete {label} ({}); freed {}.",
                error.kind(),
                format_bytes(freed)
            ))
        }
    }
}

/// Each regular file is counted once. Symlinks count only regular files whose
/// targets stay within this model's canonical folder; directory symlink targets
/// are never traversed.
fn folder_size(folder: &Path) -> u64 {
    use std::collections::HashSet;
    use std::os::unix::fs::MetadataExt;

    let Ok(root) = folder.canonicalize() else {
        return 0;
    };
    let mut seen = HashSet::new();
    let mut total = 0_u64;
    let mut stack = vec![folder.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(link) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let metadata = if link.file_type().is_symlink() {
                let Ok(target) = path.canonicalize() else {
                    continue;
                };
                if !target.starts_with(&root) {
                    continue;
                }
                let Ok(metadata) = std::fs::metadata(&target) else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                metadata
            } else if link.is_dir() {
                stack.push(path);
                continue;
            } else if link.is_file() {
                link
            } else {
                continue;
            };
            if seen.insert((metadata.dev(), metadata.ino())) {
                total = total.saturating_add(metadata.size());
            }
        }
    }
    total
}

const MAX_LEASES: usize = 64;
const MAX_LEASE_BYTES: u64 = 4096;

fn lease_pid(name: &str) -> Option<i32> {
    let stem = name.strip_suffix(".json")?;
    let mut parts = stem.split('.');
    let pid = parts.next()?;
    if pid.is_empty() || pid.len() > 10 || !pid.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if let Some(token) = parts.next() {
        if token.len() != 16
            || !token
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            || parts.next().is_some()
        {
            return None;
        }
    }
    pid.parse().ok()
}

fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if pid == std::process::id() as i32 {
        return true;
    }
    // SAFETY: signal 0 probes existence and cannot deliver a signal.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    // ESRCH means gone. EPERM and every unexpected failure are treated as
    // alive, protecting a model when liveness cannot be established.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// `(running repos, unknown-use reason)`. Unreadable live leases are unknown,
/// never ignored, so deletion fails closed.
fn live_leases(root: Option<&Path>) -> (BTreeSet<String>, String) {
    let folder = root
        .map(Path::to_path_buf)
        .unwrap_or_else(|| lf_core::home::localflow_dir().join("stt-leases"));
    let mut paths = std::fs::read_dir(folder)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    let mut running = BTreeSet::new();
    let mut unknown = String::new();
    let mut found = 0;
    for path in paths.into_iter().take(MAX_LEASES * 2) {
        if found >= MAX_LEASES {
            break;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(pid) = lease_pid(name) else {
            continue;
        };
        if !pid_alive(pid) {
            continue;
        }
        found += 1;
        match read_lease(&path, pid) {
            Some(Some(repo)) => {
                running.insert(repo);
            }
            Some(None) => {}
            None => {
                if unknown.is_empty() {
                    unknown = "A running LocalFlow process uses a speech model LocalFlow can't identify, so nothing was deleted. Quit it or restart LocalFlow first.".to_string();
                }
            }
        }
    }
    (running, unknown)
}

/// `Some(Some(repo))`: known cached model; `Some(None)`: known no local repo;
/// `None`: malformed/unreadable/unknown, which must block every deletion.
fn read_lease(path: &Path, pid: i32) -> Option<Option<String>> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_LEASE_BYTES {
        return None;
    }
    let data: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let object = data.as_object()?;
    if object.get("version")?.as_i64()? != 1
        || object.get("pid")?.as_i64()? != i64::from(pid)
        || !object.get("backend")?.is_string()
        || !object.get("known")?.is_boolean()
        || !object
            .get("repo")
            .is_some_and(|repo| repo.is_null() || repo.is_string())
    {
        return None;
    }
    if !object.get("known")?.as_bool()? {
        return None;
    }
    Some(object.get("repo")?.as_str().map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_core::dictionary::DictionaryStore;
    use lf_core::history::HistoryStore;
    use lf_providers::keys::{mask_key, TEST_FAILED, TEST_OK};
    use lf_providers::LlmConfig;
    use std::sync::Mutex;

    fn preset(id: &str) -> &'static Preset {
        get_preset(id).unwrap()
    }

    fn config(fields: &[(&str, &str)]) -> Config {
        let mut config = Config::defaults_for(false);
        for (name, value) in fields {
            config.set_field(name, Value::String((*value).to_string()));
        }
        config
    }

    #[test]
    fn contract_and_snapshot_states_are_credential_free_and_stable() {
        let page = ModelsPage::new();
        assert_eq!(page.name(), "Models");
        let empty = page.snapshot_state();
        assert_eq!(empty.providers.len(), PRESETS.len());
        assert!(empty.expanded.is_none());
        let mut seed = empty;
        seed.expanded = Some("custom".to_string());
        seed.providers[0].model = "gemma4:e4b".to_string();
        seed.quick_models = vec!["gemma4:e4b".to_string()];
        seed.quick_list_state = Some(ListState::Ready);
        seed.quick_draft = Some("typed-model".to_string());
        seed.quick_message = ModelsMessage {
            text: "Saved".to_string(),
            success: true,
        };
        seed.provider_message = ModelsMessage {
            text: "Unable to save".to_string(),
            success: false,
        };
        seed.storage
            .push(CachedModel::new("repo", "Whisper Base", 1000, ""));
        page.seed_snapshot(seed.clone());
        assert_eq!(page.snapshot_state(), seed);
        // Seeding replaces the production backend and suppresses every worker.
        assert!(page.state.borrow().suspend_workers);
        assert!(page.state.borrow().editor.is_none());
    }

    #[test]
    fn provider_state_seeds_active_config_then_keeps_drafts() {
        let config = config(&[("llm_model", "qwen2.5:7b")]);
        let mut state = State::default();
        let provider = provider_state(&mut state, preset("ollama"), &config);
        assert_eq!(provider.model, "qwen2.5:7b");
        provider.model = "edited-model".to_string();
        assert_eq!(
            provider_state(&mut state, preset("ollama"), &config).model,
            "edited-model"
        );
        assert_eq!(
            provider_state(&mut state, preset("openai"), &config).model,
            ""
        );
    }

    #[test]
    fn import_snapshot_contains_only_a_masked_key_and_survives_ticks() {
        let page = ModelsPage::new();
        page.seed_snapshot(ModelsSnapshot::default());
        let secret = "private-test-key-1234";
        let environ = BTreeMap::from([("OPENAI_API_KEY".to_string(), secret.to_string())]);
        page.seed_import_candidates(detect_imports(None, &environ).unwrap());
        let imports = page.import_snapshot();
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].masked_value, mask_key(secret));
        assert!(imports[0].selected);
        assert!(!format!("{imports:?}").contains(secret));
        page.state.borrow_mut().checked.insert(
            (
                "openai".to_string(),
                "environment: OPENAI_API_KEY".to_string(),
            ),
            false,
        );
        assert!(!page.import_snapshot()[0].selected);
        assert!(!format!("{:?}", page.snapshot_state()).contains(secret));
    }

    #[derive(Default)]
    struct FakeBackend {
        saved: Mutex<Vec<(String, String)>>,
        activation: Mutex<Option<Config>>,
    }

    impl ModelsBackend for FakeBackend {
        fn key_status(&self, provider_id: &str) -> Result<KeyStatus> {
            Ok(KeyStatus {
                has_key: provider_id == "openai",
                masked_value: if provider_id == "openai" {
                    "••••fake".to_string()
                } else {
                    String::new()
                },
            })
        }

        fn save_key(&self, provider_id: &str, value: &str) -> Result<()> {
            self.saved
                .lock()
                .unwrap()
                .push((provider_id.to_string(), value.to_string()));
            Ok(())
        }

        fn activate(
            &self,
            _provider_id: &str,
            _base_url: &str,
            model: &str,
            config: &Config,
            save: &dyn Fn(&Config) -> Result<()>,
        ) -> Result<Config> {
            let mut updated = config.clone();
            updated.set_str("llm_model", model);
            save(&updated)?;
            *self.activation.lock().unwrap() = Some(updated.clone());
            Ok(updated)
        }
    }

    #[test]
    fn injected_backend_does_not_touch_system_credentials() {
        let fake = Arc::new(FakeBackend::default());
        let page = ModelsPage::with_backend(fake.clone());
        assert_eq!(
            page.state
                .borrow()
                .backend
                .as_ref()
                .unwrap()
                .key_status("openai")
                .unwrap()
                .masked_value,
            "••••fake"
        );
        page.state
            .borrow()
            .backend
            .as_ref()
            .unwrap()
            .save_key("openai", "test-only-secret")
            .unwrap();
        assert_eq!(fake.saved.lock().unwrap().len(), 1);
    }

    #[test]
    fn use_rule_keeps_a_blank_model_hidden_and_changed_draft_visible() {
        let config = config(&[("llm_model", "gemma4:e4b")]);
        let backend = NoExternalBackend;
        assert!(!use_visible(
            &backend,
            preset("ollama"),
            &config,
            preset("ollama").base_url,
            " "
        ));
        assert!(use_visible(
            &backend,
            preset("ollama"),
            &config,
            preset("ollama").base_url,
            "edited-model"
        ));
        assert!(!use_visible(
            &backend,
            preset("ollama"),
            &config,
            preset("ollama").base_url,
            "gemma4:e4b"
        ));
    }

    #[test]
    fn stale_or_closed_listing_results_do_nothing() {
        let shared = Rc::new(RefCell::new(State {
            visible: true,
            ..State::default()
        }));
        shared.borrow_mut().quick.token = 2;
        quick_listed(&shared, 1, vec!["old".to_string()], ListState::Ready);
        assert!(shared.borrow().quick.models.is_empty());
        shared.borrow_mut().closed = true;
        quick_listed(&shared, 2, vec!["new".to_string()], ListState::Ready);
        assert!(shared.borrow().quick.models.is_empty());
    }

    #[test]
    fn provider_results_use_independent_stale_tokens() {
        let shared = Rc::new(RefCell::new(State::default()));
        shared.borrow_mut().providers.insert(
            "ollama".to_string(),
            ProviderState {
                models_token: 2,
                test_token: 3,
                ..ProviderState::default()
            },
        );
        provider_models_listed(&shared, "ollama", 1, vec!["old".to_string()]);
        assert!(shared.borrow().providers["ollama"].models.is_empty());
        provider_models_listed(&shared, "ollama", 2, vec!["new".to_string()]);
        assert_eq!(shared.borrow().providers["ollama"].models, ["new"]);
        provider_test_finished(&shared, "ollama", 2, (true, "old".to_string()));
        assert!(shared.borrow().providers["ollama"].message.text.is_empty());
        provider_test_finished(&shared, "ollama", 3, (false, "latest".to_string()));
        assert_eq!(shared.borrow().providers["ollama"].message.text, "latest");
    }

    #[test]
    fn snapshot_error_is_visible_and_unknown_ids_fail() {
        let mut page = ModelsPage::new();
        page.prepare_snapshot("models-error", Path::new("unused"))
            .unwrap();
        let snapshot = page.snapshot_state();
        assert_eq!(snapshot.expanded.as_deref(), Some("ollama"));
        let provider = snapshot
            .providers
            .iter()
            .find(|provider| provider.id == "ollama")
            .unwrap();
        assert!(!provider.message.text.is_empty() && !provider.message.success);
        assert!(page
            .prepare_snapshot("not-a-real-state", Path::new("unused"))
            .is_err());
    }

    #[test]
    fn snapshot_no_key_states_include_status_for_every_provider() {
        for preparation in [
            "models-populated",
            "models-empty",
            "models-error",
            "keys-no-keys",
            "keys-import-defaults",
            "keys-expanded-no-key",
            "keys-expanded-custom",
            "change-cloud",
        ] {
            let mut page = ModelsPage::new();
            page.prepare_snapshot(preparation, Path::new("unused"))
                .unwrap();
            let snapshot = page.snapshot_state();
            assert_eq!(snapshot.statuses.len(), PRESETS.len(), "{preparation}");
            for preset in PRESETS {
                let status = &snapshot.statuses[preset.id];
                assert!(!status.has_key, "{} / {preparation}", preset.id);
                assert!(
                    status.masked_value.is_empty(),
                    "{} / {preparation}",
                    preset.id
                );
            }
            if preparation.starts_with("keys-") {
                assert_eq!(snapshot.quick_list_state, Some(ListState::Loading));
                assert!(snapshot.quick_models.is_empty(), "{preparation}");
            }
        }
    }

    #[test]
    fn snapshot_default_ollama_lists_include_the_selected_model() {
        for preparation in ["models-parakeet", "storage", "storage-deleted"] {
            let mut page = ModelsPage::new();
            page.prepare_snapshot(preparation, Path::new("unused"))
                .unwrap();
            let snapshot = page.snapshot_state();
            assert_eq!(snapshot.quick_list_state, Some(ListState::Ready));
            assert_eq!(snapshot.quick_models, ["gemma4:e4b", "llama3.2:3b"]);
        }
    }

    #[test]
    fn snapshot_saved_keys_match_only_the_two_python_imports() {
        for preparation in [
            "keys-saved-active",
            "keys-import-results",
            "keys-test-success",
            "keys-test-failure",
            "keys-expanded-active",
            "keys-expanded-inactive",
        ] {
            let mut page = ModelsPage::new();
            page.prepare_snapshot(preparation, Path::new("unused"))
                .unwrap();
            let snapshot = page.snapshot_state();
            assert_eq!(snapshot.statuses.len(), PRESETS.len(), "{preparation}");
            for preset in PRESETS {
                let status = &snapshot.statuses[preset.id];
                let expected = match preset.id {
                    "fireworks" => Some("••••1234"),
                    "openai" => Some("••••5678"),
                    _ => None,
                };
                assert_eq!(
                    status.has_key,
                    expected.is_some(),
                    "{} / {preparation}",
                    preset.id
                );
                assert_eq!(
                    status.masked_value,
                    expected.unwrap_or(""),
                    "{} / {preparation}",
                    preset.id
                );
            }
            assert_eq!(snapshot.quick_list_state, Some(ListState::Loading));
            assert!(snapshot.quick_models.is_empty(), "{preparation}");
            assert_eq!(
                snapshot
                    .providers
                    .iter()
                    .find(|provider| provider.id == "fireworks")
                    .unwrap()
                    .model,
                "example-model",
                "{preparation}"
            );
            let debug = format!("{snapshot:?}");
            assert!(!debug.contains("fake-key-1234") && !debug.contains("fake-key-5678"));
        }
    }

    #[test]
    fn snapshot_import_sources_and_defaults_match_python() {
        for (preparation, selected) in [
            ("keys-import-results", false),
            ("keys-import-defaults", true),
        ] {
            let mut page = ModelsPage::new();
            page.prepare_snapshot(preparation, Path::new("unused"))
                .unwrap();
            assert_eq!(
                page.import_snapshot(),
                vec![
                    ImportSnapshot {
                        provider_id: "fireworks".into(),
                        source: "environment: FIREWORKS_API_KEY".into(),
                        masked_value: "••••1234".into(),
                        selected,
                    },
                    ImportSnapshot {
                        provider_id: "openai".into(),
                        source: "~/.zshrc: OPENAI_API_KEY".into(),
                        masked_value: "••••5678".into(),
                        selected,
                    },
                ],
                "{preparation}"
            );
            assert_eq!(
                page.snapshot_state().provider_message,
                ModelsMessage {
                    text: "Select the keys to save".into(),
                    success: true,
                }
            );
        }
    }

    #[test]
    fn snapshot_provider_test_results_match_python() {
        for (preparation, message) in [
            ("keys-test-success", TEST_OK),
            ("keys-test-failure", TEST_FAILED),
        ] {
            let mut page = ModelsPage::new();
            page.prepare_snapshot(preparation, Path::new("unused"))
                .unwrap();
            let snapshot = page.snapshot_state();
            assert_eq!(snapshot.expanded.as_deref(), Some("ollama"));
            let provider = snapshot
                .providers
                .iter()
                .find(|provider| provider.id == "ollama")
                .unwrap();
            assert_eq!(provider.message.text, message);
            assert_eq!(provider.message.success, preparation == "keys-test-success");
        }
    }

    #[test]
    fn keys_snapshot_configs_reset_then_follow_python_saved_flags() {
        let temp = tempfile::tempdir().unwrap();
        let services = HubServices::new_isolated(
            config(&[("stt_backend", "parakeet"), ("llm_model", "stale-model")]),
            Rc::new(
                HistoryStore::open(
                    Some(temp.path().join("history.sqlite")),
                    Some(temp.path().join("clips")),
                    true,
                )
                .unwrap(),
            ),
            Rc::new(DictionaryStore::new(Some(
                temp.path().join("dictionary.json"),
            ))),
        );
        let mut page = ModelsPage::new();
        page.state.borrow_mut().services = Some(services);
        for (preparation, saved) in [
            ("keys-no-keys", false),
            ("keys-saved-active", true),
            ("keys-import-results", true),
            ("keys-import-defaults", false),
            ("keys-test-success", true),
            ("keys-test-failure", true),
            ("keys-expanded-active", true),
            ("keys-expanded-inactive", true),
            ("keys-expanded-no-key", false),
            ("keys-expanded-custom", false),
        ] {
            page.prepare_snapshot(preparation, Path::new("unused"))
                .unwrap();
            let config = page
                .state
                .borrow()
                .services
                .as_ref()
                .expect("isolated services")
                .config();
            let expected = if saved {
                (
                    "openai-compatible",
                    "https://api.fireworks.ai/inference/v1",
                    "example-model",
                )
            } else {
                ("ollama", "http://localhost:11434", "llama3.2:3b")
            };
            assert_eq!(
                config.field_str("llm_provider"),
                Some(expected.0),
                "{preparation}"
            );
            assert_eq!(
                config.field_str("llm_base_url"),
                Some(expected.1),
                "{preparation}"
            );
            assert_eq!(
                config.field_str("llm_model"),
                Some(expected.2),
                "{preparation}"
            );
            assert_eq!(
                config.field_str("stt_backend"),
                Some("auto"),
                "{preparation}"
            );
        }
    }

    #[test]
    fn cloud_snapshot_uses_python_model_and_computes_statuses_from_config() {
        let temp = tempfile::tempdir().unwrap();
        let services = HubServices::new_isolated(
            Config::default(),
            Rc::new(
                HistoryStore::open(
                    Some(temp.path().join("history.sqlite")),
                    Some(temp.path().join("clips")),
                    true,
                )
                .unwrap(),
            ),
            Rc::new(DictionaryStore::new(Some(
                temp.path().join("dictionary.json"),
            ))),
        );
        let mut page = ModelsPage::new();
        page.state.borrow_mut().services = Some(services);
        page.prepare_snapshot("change-cloud", Path::new("unused"))
            .unwrap();
        let snapshot = page.snapshot_state();
        let config = page
            .state
            .borrow()
            .services
            .as_ref()
            .expect("isolated services")
            .config();
        assert_eq!(
            config.field_str("llm_model"),
            Some("accounts/fireworks/models/example-model")
        );
        assert!(snapshot.quick_draft.is_none());
        assert_eq!(
            provider_status(preset("fireworks"), &config, false),
            ("Active", ("cobalt", "cobalt"))
        );
        assert_eq!(
            provider_status(preset("ollama"), &config, false),
            ("Local", ("line", "ink"))
        );
        assert_eq!(
            provider_status(preset("openai"), &config, false),
            ("No key", ("muted", "muted"))
        );
        assert!(page.state.borrow().suspend_workers);
    }

    #[test]
    fn sizes_match_python_decimal_formatting() {
        assert_eq!(format_bytes(0), "0 bytes");
        assert_eq!(format_bytes(999), "999 bytes");
        assert_eq!(format_bytes(1000), "1.0 KB");
        assert_eq!(format_bytes(9_999), "10.0 KB");
        assert_eq!(format_bytes(10_000), "10 KB");
        assert_eq!(format_bytes(1_000_000), "1.0 MB");
        assert_eq!(format_bytes(10_000_000_000), "10 GB");
    }

    #[test]
    fn mlx_storage_capability_protects_both_auto_candidates() {
        assert_eq!(
            SystemModelsBackend::default().mlx_available,
            cfg!(target_arch = "aarch64")
        );
        let temp = tempfile::tempdir().unwrap();
        let mlx = "mlx-community/whisper-small-mlx";
        let fw = "Systran/faster-whisper-small";
        for repo in [mlx, fw] {
            let folder = temp.path().join(repo_folder_name(repo));
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(folder.join("weights"), b"test").unwrap();
        }
        for (provider, available) in [("auto", true), ("mlx", false), ("mlx-whisper", false)] {
            let backend = SystemModelsBackend {
                cache: Some(temp.path().to_path_buf()),
                leases: Some(temp.path().join("no-live-leases")),
                mlx_available: available,
            };
            let config = config(&[
                ("stt_backend", provider),
                ("model_size", "small"),
                ("stt_device", "auto"),
            ]);
            let models = backend.inventory(&config).unwrap();
            assert_eq!(models.len(), 2);
            assert!(models
                .iter()
                .all(|model| model.status == "Selected" && model.protected()));
            for repo in [mlx, fw] {
                assert!(backend
                    .delete_model(repo, &config)
                    .unwrap_err()
                    .to_string()
                    .contains("settings"));
                assert!(temp.path().join(repo_folder_name(repo)).exists());
            }
        }
    }

    #[test]
    fn storage_lists_only_catalog_models_and_protects_selection() {
        let temp = tempfile::tempdir().unwrap();
        let leases = temp.path().join("leases");
        let backend = SystemModelsBackend {
            cache: Some(temp.path().to_path_buf()),
            leases: Some(leases),
            mlx_available: false,
        };
        let config = config(&[
            ("stt_backend", "faster-whisper"),
            ("model_size", "base"),
            ("stt_device", "auto"),
        ]);
        let fw = temp
            .path()
            .join(repo_folder_name("Systran/faster-whisper-base"));
        let other = temp.path().join(repo_folder_name("some/unmanaged-model"));
        std::fs::create_dir_all(fw.join("snapshots/rev")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(fw.join("snapshots/rev/model.bin"), vec![0_u8; 100]).unwrap();
        std::fs::write(other.join("ignore.bin"), vec![0_u8; 500]).unwrap();
        let models = backend.inventory(&config).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].status, "Selected");
        assert_eq!(models[0].size_bytes, 100);
        assert!(backend
            .delete_model("Systran/faster-whisper-base", &config)
            .unwrap_err()
            .to_string()
            .contains("settings"));
        assert!(fw.exists() && other.exists());
    }

    #[test]
    fn storage_skips_catalog_folder_with_no_file_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let backend = SystemModelsBackend {
            cache: Some(temp.path().to_path_buf()),
            leases: Some(temp.path().join("leases")),
            mlx_available: false,
        };
        let config = config(&[
            ("stt_backend", "faster-whisper"),
            ("model_size", "base"),
            ("stt_device", "auto"),
        ]);
        let empty = temp
            .path()
            .join(repo_folder_name("Systran/faster-whisper-small"));
        let full = temp
            .path()
            .join(repo_folder_name("Systran/faster-whisper-base"));
        std::fs::create_dir_all(empty.join("snapshots/rev")).unwrap();
        std::fs::create_dir_all(full.join("snapshots/rev")).unwrap();
        std::fs::write(full.join("snapshots/rev/model.bin"), vec![0_u8; 100]).unwrap();
        let models = backend.inventory(&config).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].repo, "Systran/faster-whisper-base");
        assert_eq!(models[0].size_bytes, 100);
    }

    #[test]
    fn deletion_refuses_symlinked_model_and_unknown_live_lease() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("cache");
        let outside = temp.path().join("outside");
        let leases = temp.path().join("leases");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("model.bin"), b"test").unwrap();
        std::fs::create_dir_all(&leases).unwrap();
        std::os::unix::fs::symlink(
            &outside,
            cache.join(repo_folder_name("Systran/faster-whisper-small")),
        )
        .unwrap();
        let backend = SystemModelsBackend {
            cache: Some(cache.clone()),
            leases: Some(leases.clone()),
            mlx_available: false,
        };
        let config = config(&[
            ("stt_backend", "faster-whisper"),
            ("model_size", "base"),
            ("stt_device", "auto"),
        ]);
        assert!(backend.inventory(&config).unwrap().is_empty());
        assert!(backend
            .delete_model("Systran/faster-whisper-small", &config)
            .is_err());
        let real = cache.join(repo_folder_name("Systran/faster-whisper-small"));
        assert!(real.symlink_metadata().unwrap().file_type().is_symlink());
        std::fs::remove_file(&real).unwrap();
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("model.bin"), b"test").unwrap();
        std::fs::write(
            leases.join(format!("{}.json", std::process::id())),
            format!(
                "{{\"version\":1,\"pid\":{},\"backend\":\"unknown\",\"repo\":null,\"known\":false}}",
                std::process::id()
            ),
        )
        .unwrap();
        assert_eq!(backend.inventory(&config).unwrap()[0].status, "Unknown use");
        assert!(backend
            .delete_model("Systran/faster-whisper-small", &config)
            .unwrap_err()
            .to_string()
            .contains("can't identify"));
        assert!(real.exists());
    }

    #[test]
    fn folder_size_deduplicates_hardlinks_and_skips_external_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let model = temp.path().join("model");
        let blobs = model.join("blobs");
        let snapshots = model.join("snapshots/rev");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::create_dir_all(&snapshots).unwrap();
        std::fs::write(blobs.join("data"), vec![1_u8; 40]).unwrap();
        std::fs::hard_link(blobs.join("data"), snapshots.join("hard")).unwrap();
        std::os::unix::fs::symlink(blobs.join("data"), snapshots.join("inside")).unwrap();
        std::fs::write(temp.path().join("external"), vec![1_u8; 900]).unwrap();
        std::os::unix::fs::symlink(temp.path().join("external"), snapshots.join("outside"))
            .unwrap();
        assert_eq!(folder_size(&model), 40);
    }

    #[test]
    fn lease_names_and_contents_fail_closed() {
        assert_eq!(lease_pid("123.json"), Some(123));
        assert_eq!(lease_pid("123.0123456789abcdef.json"), Some(123));
        for name in [
            "x.json",
            "123.abc.json",
            "123.0123456789ABCDEf.json",
            ".json",
        ] {
            assert_eq!(lease_pid(name), None);
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("123.json");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_lease(&path, 123), None);
    }
}
