//! Models & keys → Speech: which speech models are on this computer, which
//! can be downloaded, and deleting unused ones.
//!
//! Real storage (runtime feature) is `lf_stt::model_storage`: the
//! Python-compatible Hugging Face cache, its catalogue of managed repos,
//! `model_folder` (no symlinks, must sit directly in the cache) and
//! `delete_unused`, which re-checks at delete time that the repo is neither
//! the configured model nor leased by a running LocalFlow (`stt_lease`).
//! Download state per catalogue model comes from `SetupModelBackend`
//! (`FlowModelBackend::speech_downloads` / `download_speech`). Opening the
//! page only reads local files; the network is used only after Download.

use std::collections::HashMap;
use std::path::PathBuf;

use lf_core::config::Config;
use lf_ui::setup_flow::SttModel;

use crate::controllers::jobs::{self, Downloads, JobState};
use crate::controllers::setup_flow::Loadable;

/// One managed repository folder in the model cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredModel {
    pub repo: String,
    pub label: String,
    pub size_bytes: u64,
    /// `""`, `"Selected"`, `"In use"` or `"Unknown use"` (protected).
    pub status: String,
}

impl StoredModel {
    pub fn protected(&self) -> bool {
        !self.status.is_empty()
    }

    /// Why Delete is disabled, if it is.
    pub fn protection_reason(&self) -> Option<&'static str> {
        match self.status.as_str() {
            "" => None,
            "Selected" => Some("Selected in settings. Choose another model first."),
            "In use" => Some("In use by a running LocalFlow. Stop dictation or quit it first."),
            _ => Some("A running LocalFlow uses a model LocalFlow can't identify. Quit it first."),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inventory {
    pub cache_dir: Option<PathBuf>,
    pub models: Vec<StoredModel>,
}

impl Inventory {
    pub fn total_bytes(&self) -> u64 {
        self.models.iter().map(|m| m.size_bytes).sum()
    }
}

pub trait SpeechStorage: Send + Sync {
    /// Local inspection only (no network).
    fn inventory(&self, config: &Config) -> Result<Inventory, String>;
    /// Delete one managed repo folder after re-checking protection.
    /// Returns the bytes freed.
    fn delete(&self, repo: &str, config: &Config) -> Result<u64, String>;
}

/// `lf_stt::model_storage::format_bytes`-compatible sizes ("1.5 GB").
pub fn format_bytes(bytes: u64) -> String {
    let mut value = bytes as f64;
    for unit in ["bytes", "KB", "MB", "GB"] {
        if value < 1000.0 || unit == "GB" {
            return if unit == "bytes" {
                format!("{bytes} bytes")
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

/// The real model cache (runtime feature only).
#[cfg(feature = "runtime")]
pub struct CacheStorage {
    pub cache: Option<PathBuf>,
    pub leases: Option<PathBuf>,
}

#[cfg(feature = "runtime")]
impl CacheStorage {
    pub fn user() -> Self {
        CacheStorage {
            cache: None,
            leases: None,
        }
    }

    fn cache(&self) -> PathBuf {
        self.cache
            .clone()
            .unwrap_or_else(|| lf_stt::model_storage::hf_hub_cache(None))
    }

    fn leases(&self) -> Vec<lf_stt::stt_lease::Lease> {
        lf_stt::stt_lease::live_leases(self.leases.as_deref(), lf_stt::stt_lease::pid_alive)
    }
}

#[cfg(feature = "runtime")]
impl SpeechStorage for CacheStorage {
    fn inventory(&self, config: &Config) -> Result<Inventory, String> {
        let cache = self.cache();
        if cache.exists() && std::fs::read_dir(&cache).is_err() {
            return Err(format!(
                "Couldn't read the model folder {}",
                cache.display()
            ));
        }
        let leases = self.leases();
        let models = lf_stt::model_storage::inventory(
            config,
            Some(&cache),
            lf_stt::model_storage::default_mlx_available,
            Some(leases.as_slice()),
        )
        .into_iter()
        .map(|m| StoredModel {
            repo: m.repo,
            label: m.label,
            size_bytes: m.size_bytes,
            status: m.status,
        })
        .collect();
        Ok(Inventory {
            cache_dir: Some(cache),
            models,
        })
    }

    fn delete(&self, repo: &str, config: &Config) -> Result<u64, String> {
        // Leases are read again here: a runtime that started meanwhile wins.
        let leases = self.leases();
        lf_stt::model_storage::delete_unused(
            repo,
            config,
            Some(&self.cache()),
            lf_stt::model_storage::default_mlx_available,
            Some(leases.as_slice()),
        )
        .map_err(|e| jobs::one_line(&e.to_string()))
    }
}

/// Delete `repo` re-checking protection against the config as it is *now*:
/// `load` re-reads the explicit config inside the `config_write` transaction
/// that in-app selection changes also take, so a model selected while the
/// delete waited is protected. Blocking; call on a worker.
pub fn delete_with_current_config(
    storage: &dyn SpeechStorage,
    repo: &str,
    load: impl FnOnce() -> anyhow::Result<Config>,
) -> Result<u64, String> {
    lf_ui::config_write::with_lock(|| {
        let config = load().map_err(|e| format!("Couldn't read your settings: {e:#}"))?;
        storage.delete(repo, &config)
    })
}

/// Default build: no model storage code is linked.
pub struct UnavailableStorage {
    pub reason: String,
}

impl SpeechStorage for UnavailableStorage {
    fn inventory(&self, _: &Config) -> Result<Inventory, String> {
        Err(self.reason.clone())
    }
    fn delete(&self, _: &str, _: &Config) -> Result<u64, String> {
        Err(self.reason.clone())
    }
}

/// What a catalogue model's row shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowState {
    NotInstalled,
    Downloading(JobState),
    Failed(String),
    Installed,
    Active,
    Unavailable(String),
    Unknown(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRow {
    pub id: String,
    pub label: String,
    pub size_mb: u32,
    pub blurb: &'static str,
    pub selected: bool,
    pub recommended: bool,
    pub state: RowState,
}

/// Rows for the catalogue models this machine can run.
pub fn model_rows(
    models: &[SttModel],
    installed: &Loadable<HashMap<String, bool>>,
    downloads: &Downloads,
    selected_id: &str,
    recommended: &str,
    unavailable: Option<&str>,
) -> Vec<ModelRow> {
    models
        .iter()
        .map(|model| {
            let job = downloads.state(&jobs::speech_key(&model.id));
            let selected = model.id == selected_id;
            let state = if let Some(reason) = unavailable {
                RowState::Unavailable(reason.to_string())
            } else {
                match (job, installed) {
                    (Some(job @ JobState::Running { .. }), _) => RowState::Downloading(job),
                    (Some(JobState::Failed(error)), _) => RowState::Failed(error),
                    (_, Loadable::Failed(error)) => RowState::Unknown(error.clone()),
                    (_, Loadable::Ready(map)) => match map.get(&model.id) {
                        Some(true) if selected => RowState::Active,
                        Some(true) => RowState::Installed,
                        Some(false) => RowState::NotInstalled,
                        None => RowState::Unknown("Not inspected.".into()),
                    },
                    (_, _) => RowState::Unknown("Checking…".into()),
                }
            };
            ModelRow {
                id: model.id.clone(),
                label: format!("{} · {}", engine_label(model.backend), model.label),
                size_mb: model.size_mb,
                blurb: model.blurb,
                selected,
                recommended: model.id == recommended,
                state,
            }
        })
        .collect()
}

pub fn engine_label(backend: &str) -> &'static str {
    match backend {
        "parakeet" => "Parakeet",
        "mlx-whisper" => "MLX Whisper",
        _ => "Whisper",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_ui::setup_flow::stt_model_by_id;

    fn config(backend: &str) -> Config {
        let mut c = Config::defaults_for(false);
        c.set_field("stt_backend", serde_json::json!(backend));
        c
    }

    #[test]
    fn every_row_state_comes_from_inventory_and_jobs() {
        let models: Vec<SttModel> = ["fw-tiny", "fw-base", "fw-small", "fw-medium"]
            .iter()
            .map(|id| stt_model_by_id(id).unwrap())
            .collect();
        let downloads = Downloads::new();
        let mut map = HashMap::new();
        map.insert("fw-base".to_string(), true);
        map.insert("fw-small".to_string(), true);
        map.insert("fw-tiny".to_string(), false);
        let (go, gate) = std::sync::mpsc::channel::<()>();
        downloads.start(
            &jobs::speech_key("fw-medium"),
            Box::new(move |_, _| {
                let _ = gate.recv();
                Ok(())
            }),
        );
        let rows = model_rows(
            &models,
            &Loadable::Ready(map.clone()),
            &downloads,
            "fw-base",
            "fw-base",
            None,
        );
        assert_eq!(rows[0].state, RowState::NotInstalled);
        assert_eq!(rows[1].state, RowState::Active);
        assert!(rows[1].recommended);
        assert_eq!(rows[2].state, RowState::Installed);
        assert!(matches!(rows[3].state, RowState::Downloading(_)));
        go.send(()).unwrap();
        downloads.wait_idle(std::time::Duration::from_secs(3));
        downloads.start(
            &jobs::speech_key("fw-medium"),
            Box::new(|_, _| Err("disk full".into())),
        );
        downloads.wait_idle(std::time::Duration::from_secs(3));
        let rows = model_rows(
            &models,
            &Loadable::Ready(map),
            &downloads,
            "fw-base",
            "fw-base",
            None,
        );
        assert_eq!(rows[3].state, RowState::Failed("disk full".into()));
        let rows = model_rows(
            &models,
            &Loadable::Failed("unreadable".into()),
            &Downloads::new(),
            "fw-base",
            "",
            None,
        );
        assert_eq!(rows[0].state, RowState::Unknown("unreadable".into()));
        let rows = model_rows(
            &models,
            &Loadable::Idle,
            &Downloads::new(),
            "",
            "",
            Some("Rebuild"),
        );
        assert!(rows
            .iter()
            .all(|r| r.state == RowState::Unavailable("Rebuild".into())));
    }

    #[test]
    fn deletion_rechecks_the_selection_made_while_it_waited() {
        let dir = tempfile::tempdir().unwrap();
        let root = crate::data::DataRoot::sandbox(dir.path());
        let file = crate::data::ConfigFile::new(root.clone());
        file.apply(&serde_json::json!({"stt": {"provider": "parakeet"}}))
            .unwrap();
        let stale = file.load().unwrap(); // what the page had when Delete was clicked
        let storage = crate::sandbox::SimStorage::new(crate::sandbox::SimSpeech::seeded());
        // The user selects a Whisper model before the queued worker runs.
        file.apply(&serde_json::json!({"stt": {"provider": "faster-whisper", "model": "small"}}))
            .unwrap();
        assert!(
            storage.inventory(&stale).unwrap().models[0]
                .status
                .is_empty(),
            "stale view: unprotected"
        );
        let refused =
            delete_with_current_config(&storage, crate::sandbox::lf_stt_repo::WHISPER_CPP, || {
                crate::data::load_config_at(&root)
            });
        assert!(
            refused.unwrap_err().contains("Choose another"),
            "current selection protects it"
        );
        assert!(!storage
            .inventory(&file.load().unwrap())
            .unwrap()
            .models
            .is_empty());
    }

    #[test]
    fn unavailable_storage_never_pretends() {
        let storage = UnavailableStorage {
            reason: "rebuild with --features runtime".into(),
        };
        assert!(storage.inventory(&config("auto")).is_err());
        assert!(storage
            .delete("ggerganov/whisper.cpp", &config("auto"))
            .is_err());
        assert_eq!(format_bytes(1_480_000_000), "1.5 GB");
        assert_eq!(format_bytes(486_000_000), "486 MB");
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn real_cache_storage_validates_paths_and_protects_selected_and_leased_repos() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("hub");
        let leases = root.path().join("leases");
        std::fs::create_dir_all(&leases).unwrap();
        let storage = CacheStorage {
            cache: Some(cache.clone()),
            leases: Some(leases.clone()),
        };
        let repo = "Systran/faster-whisper-small";
        let folder = cache.join(lf_stt::model_storage::repo_folder_name(repo));
        std::fs::create_dir_all(folder.join("snapshots/x")).unwrap();
        std::fs::write(folder.join("snapshots/x/model.bin"), vec![0u8; 2048]).unwrap();
        let cfg = config("parakeet");
        let inventory = storage.inventory(&cfg).unwrap();
        let stored = inventory.models.iter().find(|m| m.repo == repo).unwrap();
        assert_eq!(stored.size_bytes, 2048);
        assert!(!stored.protected());
        // The configured model is kept.
        let mut selected = config("faster-whisper");
        selected.set_field("model_size", serde_json::json!("small"));
        assert!(storage.delete(repo, &selected).is_err());
        assert!(folder.exists());
        // Not a managed repo / traversal: refused.
        assert!(storage.delete("../outside", &cfg).is_err());
        assert_eq!(storage.delete(repo, &cfg).unwrap(), 2048);
        assert!(!folder.exists());
        assert!(
            storage.delete(repo, &cfg).is_err(),
            "nothing left to delete"
        );
    }
}
