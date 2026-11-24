//! Explicit data locations and the services every page uses.
//!
//! Every store is opened with a path taken from [`DataRoot`]; no default-path
//! constructor is used anywhere in this crate. That is what keeps `--sandbox`
//! away from real user data.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use lf_core::config::{self, Config};
use lf_core::dictionary::DictionaryStore;
use lf_core::history::HistoryStore;
use lf_core::hotphrases::HotPhraseStore;
use lf_core::phrases_model::{PhrasesModel, PhrasesSettings};
use lf_providers::{ConfigStore, SecretStore};
use serde_json::Value;

use lf_ui::backends::settings_agent::SettingsAgent;

use crate::controllers::capabilities::Capabilities;
use crate::controllers::jobs::Downloads;
use crate::controllers::model_storage::SpeechStorage;
use crate::controllers::retry::{RetryBackend, RetryJobs};
use crate::controllers::runtime_state::SimulatedState;
use crate::controllers::setup_flow::SetupDeps;
use crate::remote::Remote;

/// Real data or an isolated sandbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Real,
    Sandbox,
}

/// Where every file lives for this run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataRoot {
    pub mode: Mode,
    /// Sandbox base directory (None in real mode).
    pub sandbox_dir: Option<PathBuf>,
    pub config_path: PathBuf,
    pub localflow_dir: PathBuf,
    pub history_path: PathBuf,
    pub clips_dir: PathBuf,
    pub dictionary_path: PathBuf,
    pub hot_phrases_path: PathBuf,
    pub token_path: PathBuf,
}

/// Marker written into a seeded sandbox directory.
pub const SANDBOX_MARKER: &str = "LOCALFLOW-DESKTOP-SANDBOX.txt";

impl DataRoot {
    /// The user's real LocalFlow locations (`LOCALFLOW_CONFIG`,
    /// `XDG_CONFIG_HOME` and `~/.localflow` resolved by `lf_core`).
    pub fn real() -> Result<Self> {
        let config_path = config::try_config_path()?;
        let localflow_dir = lf_core::home::localflow_dir();
        let token_path = lf_core::apitoken::try_token_path()?;
        Ok(Self::under(
            Mode::Real,
            None,
            config_path,
            localflow_dir,
            token_path,
        ))
    }

    /// An isolated sandbox rooted at `dir`. Nothing outside `dir` is used.
    pub fn sandbox(dir: &Path) -> Self {
        let dir = dir.to_path_buf();
        let localflow_dir = dir.join("localflow");
        Self::under(
            Mode::Sandbox,
            Some(dir.clone()),
            dir.join("config").join("config.toml"),
            localflow_dir.clone(),
            localflow_dir.join("api-token"),
        )
    }

    fn under(
        mode: Mode,
        sandbox_dir: Option<PathBuf>,
        config_path: PathBuf,
        localflow_dir: PathBuf,
        token_path: PathBuf,
    ) -> Self {
        DataRoot {
            mode,
            sandbox_dir,
            history_path: localflow_dir.join("history.sqlite"),
            clips_dir: localflow_dir.join("clips"),
            dictionary_path: localflow_dir.join("dictionary.json"),
            hot_phrases_path: localflow_dir.join("hot_phrases.json"),
            config_path,
            localflow_dir,
            token_path,
        }
    }

    pub fn is_sandbox(&self) -> bool {
        self.mode == Mode::Sandbox
    }

    /// Every path this root may write.
    pub fn all_paths(&self) -> Vec<&Path> {
        vec![
            &self.config_path,
            &self.localflow_dir,
            &self.history_path,
            &self.clips_dir,
            &self.dictionary_path,
            &self.hot_phrases_path,
            &self.token_path,
        ]
    }
}

/// Default persistent sandbox directory: `$XDG_DATA_HOME/localflow-desktop-sandbox`
/// (never `~/.localflow` and never the real config directory).
pub fn default_sandbox_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("localflow-desktop-sandbox")
}

/// Load the config at `path`; a missing file gives defaults (real mode also
/// honours legacy migration through `load_config` when `path` is the default).
pub fn load_config_at(root: &DataRoot) -> Result<Config> {
    if root.config_path.exists() {
        return config::load_from(&root.config_path);
    }
    if root.mode == Mode::Real && config::try_config_path().ok().as_ref() == Some(&root.config_path)
    {
        return config::load_config();
    }
    let mut defaults = Config::defaults_for(cfg!(target_os = "macos"));
    if !cfg!(target_os = "macos") {
        defaults.set_field("meetings_enabled", Value::Bool(false));
    }
    Ok(defaults)
}

/// Config edits through `lf_ui::config_write::update` at an explicit path.
///
/// Each change reloads the latest file, validates the sectioned update with
/// `lf_core::config::apply_config_update` and saves atomically, so fields the
/// edit does not touch (including ones another process changed) are kept.
#[derive(Clone, Debug)]
pub struct ConfigFile {
    pub root: DataRoot,
}

impl ConfigFile {
    pub fn new(root: DataRoot) -> Self {
        ConfigFile { root }
    }

    pub fn path(&self) -> &Path {
        &self.root.config_path
    }

    pub fn load(&self) -> Result<Config> {
        load_config_at(&self.root)
    }

    /// Make sure the file exists so `config_write::update` can reload it.
    pub fn ensure_file(&self) -> Result<()> {
        if !self.root.config_path.exists() {
            let current = self.load()?;
            config::save_config(&current, Some(&self.root.config_path))?;
        }
        Ok(())
    }

    /// Apply a sectioned update such as `{"dictation": {"enabled": false}}`.
    pub fn apply(&self, update: &Value) -> Result<Config> {
        self.ensure_file()?;
        lf_ui::config_write::update(Some(&self.root.config_path), |latest| {
            config::apply_config_update(latest, update)
        })
    }

    /// Run an arbitrary validated transformation of the latest config.
    pub fn update(&self, change: impl FnOnce(&Config) -> Result<Config>) -> Result<Config> {
        self.ensure_file()?;
        lf_ui::config_write::update(Some(&self.root.config_path), change)
    }
}

impl ConfigStore<Config> for ConfigFile {
    fn load(&self) -> Result<Config> {
        ConfigFile::load(self)
    }
    fn save(&self, updated: &Config) -> Result<()> {
        self.ensure_file()?;
        lf_ui::config_write::with_lock(|| {
            config::save_config(updated, Some(&self.root.config_path)).map(|_| ())
        })
    }
}

/// In-memory secrets for the sandbox (the OS keyring is never touched).
#[derive(Default)]
pub struct MemorySecrets(Mutex<BTreeMap<String, String>>);

impl SecretStore for MemorySecrets {
    fn get(&self, name: &str) -> String {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
            .unwrap_or_default()
    }
    fn set(&self, name: &str, value: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.into(), value.into());
        Ok(())
    }
    fn delete(&self, name: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name)
            .map(|_| ())
            .ok_or_else(|| anyhow::anyhow!("no such secret"))
    }
}

/// Everything a page needs. Lives on the UI thread.
pub struct Services {
    pub root: DataRoot,
    pub config_file: ConfigFile,
    config: RefCell<Config>,
    pub config_error: RefCell<Option<String>>,
    pub dictionary: DictionaryStore,
    pub secrets: Arc<dyn SecretStore + Send + Sync>,
    pub remote: Remote,
    /// What this launch may run (sandbox / runtime feature / `--engine`).
    pub caps: Capabilities,
    /// Model downloads shared by setup and Models & keys.
    pub downloads: Downloads,
    /// Speech-model cache inventory and delete.
    pub storage: Arc<dyn SpeechStorage>,
    /// Setup wizard platform, model backend, meter and launch agent.
    pub setup: SetupDeps,
    /// The settings assistant's agent (explicit paths; sandbox: scripted).
    pub assistant: Arc<dyn SettingsAgent>,
    /// Sandbox-only: which runtime state Home simulates.
    pub simulated_runtime: std::cell::Cell<SimulatedState>,
    /// History retry models: native (runtime), unavailable, or simulated.
    pub retry: Arc<dyn RetryBackend>,
    /// The process's one History retry job and its last result.
    pub retry_jobs: RetryJobs,
}

/// Storage, setup, Settings assistant and History retry backends, composed
/// together so sandbox and real runs pick one consistent set.
type Backends = (
    Arc<dyn SpeechStorage>,
    SetupDeps,
    Arc<dyn SettingsAgent>,
    Arc<dyn RetryBackend>,
);

impl Services {
    pub fn open(root: DataRoot) -> Result<Self> {
        let caps = Capabilities::new(root.is_sandbox(), false);
        Self::open_with(root, caps)
    }

    /// Open with explicit launch capabilities (`--engine`). The sandbox
    /// always gets fakes, whatever `caps` asks for.
    pub fn open_with(root: DataRoot, caps: Capabilities) -> Result<Self> {
        let caps = Capabilities {
            sandbox: root.is_sandbox(),
            ..caps
        };
        if root.is_sandbox() {
            // Belt and braces: any code path reaching lf_core::secrets uses memory.
            lf_core::secrets::set_keyring_backend(Some(Arc::new(
                lf_core::secrets::MemoryKeyring::new(),
            )));
        }
        let config_file = ConfigFile::new(root.clone());
        let (config, error) = match config_file.load() {
            Ok(config) => (config, None),
            Err(err) => (
                Config::defaults_for(cfg!(target_os = "macos")),
                Some(format!("{err:#}")),
            ),
        };
        let secrets: Arc<dyn SecretStore + Send + Sync> = if root.is_sandbox() {
            Arc::new(MemorySecrets::default())
        } else {
            Arc::new(lf_providers::CoreSecrets)
        };
        let remote = Remote::for_root(&root);
        let undo_path = root.localflow_dir.join("settings-undo.json");
        let (storage, setup, assistant, retry): Backends = if root.is_sandbox() {
            // One simulated model inventory behind storage, downloads, the
            // setup wizard, Ollama pulls and History retry, so they agree.
            let sim = crate::sandbox::SimSpeech::seeded();
            (
                Arc::new(crate::sandbox::SimStorage::new(sim.clone())),
                SetupDeps {
                    platform: Arc::new(crate::sandbox::setup_platform()),
                    models: Arc::new(crate::sandbox::SimSetupBackend::with_state(
                        sim.clone(),
                        std::time::Duration::from_millis(100),
                    )),
                    meter: Arc::new(crate::sandbox::SimMeterInput),
                    launch_agent: Arc::new(lf_ui::setup_flow::FakeLaunchAgent::new(
                        "not installed",
                    )),
                    simulated: true,
                    native_speech: true,
                    speech_reason: None,
                },
                crate::controllers::assistant::sandbox_agent(
                    root.config_path.clone(),
                    root.dictionary_path.clone(),
                    undo_path,
                ),
                Arc::new(crate::sandbox::SimRetry::new(sim)),
            )
        } else {
            (
                real_storage(&caps),
                SetupDeps {
                    platform: Arc::new(crate::controllers::setup_flow::DesktopPlatform::new()),
                    models: Arc::new(lf_ui::setup_backend::FlowModelBackend::default()),
                    meter: Arc::new(crate::controllers::setup_flow::AudioMeterInput),
                    launch_agent: Arc::new(lf_ui::setup_flow::UnavailableLaunchAgent),
                    simulated: false,
                    native_speech: caps.native_speech(),
                    speech_reason: caps.native_speech_reason(),
                },
                crate::controllers::assistant::native_agent(
                    root.config_path.clone(),
                    root.dictionary_path.clone(),
                    undo_path,
                ),
                real_retry(&secrets),
            )
        };
        Ok(Services {
            retry,
            retry_jobs: RetryJobs::new(),
            dictionary: DictionaryStore::new(Some(root.dictionary_path.clone())),
            config_file,
            config: RefCell::new(config),
            config_error: RefCell::new(error),
            secrets,
            remote,
            caps,
            downloads: Downloads::new(),
            storage,
            setup,
            assistant,
            simulated_runtime: std::cell::Cell::new(SimulatedState::Example),
            root,
        })
    }

    pub fn config(&self) -> Config {
        self.config.borrow().clone()
    }

    pub fn is_sandbox(&self) -> bool {
        self.root.is_sandbox()
    }

    /// Re-read the config file (after an external change or a failed write).
    pub fn reload_config(&self) {
        match self.config_file.load() {
            Ok(config) => {
                *self.config.borrow_mut() = config;
                *self.config_error.borrow_mut() = None;
            }
            Err(err) => *self.config_error.borrow_mut() = Some(format!("{err:#}")),
        }
    }

    /// Apply a sectioned config update; the cached config follows the file.
    pub fn apply(&self, update: &Value) -> Result<Config> {
        let updated = self.config_file.apply(update)?;
        *self.config.borrow_mut() = updated.clone();
        Ok(updated)
    }

    pub fn update(&self, change: impl FnOnce(&Config) -> Result<Config>) -> Result<Config> {
        let updated = self.config_file.update(change)?;
        *self.config.borrow_mut() = updated.clone();
        Ok(updated)
    }

    pub fn set_cached_config(&self, config: Config) {
        *self.config.borrow_mut() = config;
    }

    /// History store at the explicit path (opened per use; cheap).
    pub fn history(&self) -> Result<HistoryStore> {
        open_history(&self.root)
    }

    pub fn phrases_model(&self) -> Result<PhrasesModel> {
        let settings = PhrasesSettings::from_config(&self.config());
        PhrasesModel::new(
            HotPhraseStore::new(self.root.hot_phrases_path.clone()),
            Some(settings),
        )
        .map(|model| {
            model.with_dictionary(DictionaryStore::new(Some(
                self.root.dictionary_path.clone(),
            )))
        })
    }
}

#[cfg(feature = "runtime")]
fn real_storage(_: &Capabilities) -> Arc<dyn SpeechStorage> {
    Arc::new(crate::controllers::model_storage::CacheStorage::user())
}

#[cfg(not(feature = "runtime"))]
fn real_storage(caps: &Capabilities) -> Arc<dyn SpeechStorage> {
    Arc::new(crate::controllers::model_storage::UnavailableStorage {
        reason: caps.native_speech_reason().unwrap_or_default(),
    })
}

#[cfg(feature = "runtime")]
fn real_retry(secrets: &Arc<dyn SecretStore + Send + Sync>) -> Arc<dyn RetryBackend> {
    Arc::new(crate::controllers::retry::NativeRetry {
        secrets: secrets.clone(),
    })
}

#[cfg(not(feature = "runtime"))]
fn real_retry(_: &Arc<dyn SecretStore + Send + Sync>) -> Arc<dyn RetryBackend> {
    Arc::new(crate::controllers::retry::UnavailableRetry)
}

/// Open history for reading and editing regardless of `[history] enabled`
/// (that flag stops new dictations from being saved; existing rows stay
/// viewable and deletable).
pub fn open_history(root: &DataRoot) -> Result<HistoryStore> {
    HistoryStore::open(
        Some(root.history_path.clone()),
        Some(root.clips_dir.clone()),
        true,
    )
    .with_context(|| format!("open {}", root.history_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sandbox_root_never_points_at_real_locations() {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::sandbox(dir.path());
        let home = dirs::home_dir().unwrap();
        let real_lf = home.join(".localflow");
        let real_cfg = config::try_config_dir().unwrap();
        for path in root.all_paths() {
            assert!(path.starts_with(dir.path()), "{path:?} escapes the sandbox");
            assert!(!path.starts_with(&real_lf));
            assert!(!path.starts_with(&real_cfg));
        }
        assert!(!default_sandbox_dir().starts_with(&real_lf));
        assert!(!default_sandbox_dir().starts_with(&real_cfg));
    }

    #[test]
    fn config_updates_merge_and_preserve_unrelated_fields() {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::sandbox(dir.path());
        let file = ConfigFile::new(root.clone());
        // First write creates the file from defaults.
        let saved = file
            .apply(&json!({"dictation": {"enabled": false}}))
            .unwrap();
        assert_eq!(saved.field_bool("dictation_enabled"), Some(false));
        // Another process edits an unrelated key meanwhile.
        let mut external = config::load_from(&root.config_path).unwrap();
        external.set_field("llm_model", json!("other-model"));
        config::save_config(&external, Some(&root.config_path)).unwrap();
        let saved = file
            .apply(&json!({"cleanup": {"level": "medium"}}))
            .unwrap();
        assert_eq!(saved.field_str("cleanup_level"), Some("medium"));
        assert_eq!(saved.field_str("llm_model"), Some("other-model"));
        assert_eq!(saved.field_bool("dictation_enabled"), Some(false));
    }

    #[test]
    fn invalid_config_updates_leave_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let file = ConfigFile::new(DataRoot::sandbox(dir.path()));
        file.apply(&json!({"cleanup": {"level": "light"}})).unwrap();
        let before = std::fs::read(file.path()).unwrap();
        assert!(file.apply(&json!({"cleanup": {"level": "loud"}})).is_err());
        assert!(file.apply(&json!({"nope": {"x": 1}})).is_err());
        assert!(file.apply(&json!({"history": {"enabled": "yes"}})).is_err());
        assert_eq!(std::fs::read(file.path()).unwrap(), before);
    }

    #[test]
    fn sandbox_services_use_only_fakes_whatever_the_launch_asks() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::sandbox::prepare(&dir.path().join("sb"), false).unwrap();
        // Even a runtime build asked for `--engine` gets no native runtime.
        let caps = Capabilities {
            sandbox: false,
            runtime_compiled: true,
            engine_requested: true,
        };
        let services = Services::open_with(root.clone(), caps).unwrap();
        assert!(services.caps.sandbox);
        assert!(!services.caps.engine_allowed());
        assert!(!services.caps.real_microphone());
        assert!(services.setup.simulated);
        assert!(services.setup.platform.input_devices().unwrap()[0].contains("example"));
        // Storage is in memory: no cache directory, no disk inventory.
        let inventory = services.storage.inventory(&services.config()).unwrap();
        assert!(inventory.cache_dir.is_none());
        // Ask uses in-process fake tools and model: no MCP server is started.
        let agent = services.remote.agent(
            services.config(),
            services.secrets.clone(),
            std::time::Duration::ZERO,
        );
        assert!(agent.simulated());
        assert_eq!(agent.model(1).unwrap().id(), "sandbox");
        // Secrets never reach the OS keyring: a test-only entry (not any
        // provider's key name, so a wiring regression can't clobber a real
        // credential) round-trips through the in-memory store. The marker
        // is this test's own temporary path, not a credential.
        let entry = "lf_desktop_test_sandbox_roundtrip";
        let marker = root.localflow_dir.display().to_string();
        assert_eq!(services.secrets.get(entry), "", "starts empty");
        services.secrets.set(entry, &marker).unwrap();
        assert_eq!(services.secrets.get(entry), marker);
        services.secrets.delete(entry).unwrap();
        assert_eq!(services.secrets.get(entry), "");
        // Every path the sandbox writes stays inside it, the window lock too.
        let instance = crate::controllers::instance::instance_dir(&services.root);
        assert!(instance.starts_with(dir.path()));
        for path in services.root.all_paths() {
            assert!(path.starts_with(dir.path()));
        }
    }

    #[test]
    fn memory_secrets_round_trip() {
        let secrets = MemorySecrets::default();
        assert_eq!(secrets.get("a"), "");
        secrets.set("a", "b").unwrap();
        assert_eq!(secrets.get("a"), "b");
        secrets.delete("a").unwrap();
        assert!(secrets.delete("a").is_err());
    }
}
