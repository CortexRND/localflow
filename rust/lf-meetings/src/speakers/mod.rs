//! Meeting speaker data shared with the Python runtime (slices B8/B9, Rust R4):
//! per-meeting sidecars under `~/.localflow/meetings`, the opt-in voiceprint
//! registry under `~/.localflow/speakers` (`registry.json` + `<id>.npy`), the
//! export ZIP, and the in-vault transcript relabel.
//!
//! The on-disk format is `localflow/diarization/{store,registry,staging}.py`'s.
//! Like Python, one server process is the single writer per data directory (the
//! CLIs go through its HTTP API); the two runtimes share a format, not a
//! concurrent-writer guarantee. [`SpeakerData`] holds that process's two
//! locks (always taken sidecar → registry) and the forget epoch.
//!
//! Nothing here logs a name, a path from a request, or a vector.

pub mod bundle;
pub mod npy;
pub mod registry;
pub mod relabel;
pub mod store;

use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex, MutexGuard,
    },
};

pub use registry::{Registry, RegistryError, Voiceprint};
pub use store::MeetingSpeakerStore;

/// The sidecar lock: held around every sidecar read-modify-write.
pub struct SidecarLock<'a>(#[allow(dead_code)] MutexGuard<'a, ()>);
/// The registry lock: held around every registry read-modify-write.
pub struct RegistryLock<'a>(#[allow(dead_code)] MutexGuard<'a, ()>);

/// The speaker data of one server process: roots, locks and forget epoch.
#[derive(Default)]
pub struct SpeakerData {
    meetings_root: Option<PathBuf>,
    speakers_root: Option<PathBuf>,
    sidecars: Mutex<()>,
    registry: Mutex<()>,
    epoch: AtomicU64,
    warned: Mutex<HashSet<String>>,
}

impl SpeakerData {
    /// `~/.localflow/{meetings,speakers}`, resolved at each use.
    pub fn new() -> Self {
        Self::default()
    }
    /// Fixed roots (tests).
    pub fn with_roots(meetings: impl Into<PathBuf>, speakers: impl Into<PathBuf>) -> Self {
        Self {
            meetings_root: Some(meetings.into()),
            speakers_root: Some(speakers.into()),
            ..Self::default()
        }
    }
    pub fn meetings_root(&self) -> PathBuf {
        self.meetings_root
            .clone()
            .unwrap_or_else(|| lf_core::home::localflow_dir().join("meetings"))
    }
    pub fn speakers_root(&self) -> PathBuf {
        self.speakers_root
            .clone()
            .unwrap_or_else(default_speakers_root)
    }
    pub fn store(&self) -> MeetingSpeakerStore<'_> {
        MeetingSpeakerStore::new(self, self.meetings_root())
    }
    pub fn registry(&self) -> Registry<'_> {
        Registry::new(self, self.speakers_root())
    }
    pub fn lock_sidecars(&self) -> SidecarLock<'_> {
        SidecarLock(self.sidecars.lock().unwrap_or_else(|e| e.into_inner()))
    }
    pub fn lock_registry(&self) -> RegistryLock<'_> {
        RegistryLock(self.registry.lock().unwrap_or_else(|e| e.into_inner()))
    }
    /// Incremented by every forget; read without a lock.
    pub fn forget_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
    pub(crate) fn bump_epoch(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }
    /// True the first time `key` is seen (one warning per reason).
    pub(crate) fn first_warning(&self, key: String) -> bool {
        self.warned
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key)
    }
}

/// Config flags reported with the model status (`models.FLAG_NAMES`).
pub const FLAG_NAMES: [&str; 3] = ["speaker_diarization", "diarize_mic", "speaker_recognition"];

/// The one gate for every voiceprint read or write: both flags on.
pub fn recognition_on(config: &lf_core::config::Config) -> bool {
    config.field_bool("speaker_diarization") == Some(true)
        && config.field_bool("speaker_recognition") == Some(true)
}

/// Offline readiness of the speaker models in the hub `cache` and of this
/// binary's runtime, plus the flags: `models.status()` + `models.flags()`.
/// Read-only: a file that fails verification is reported, never removed.
pub fn diarization_status(cache: &Path, config: &lf_core::config::Config) -> serde_json::Value {
    use lf_diarize::models::{verified_path, DIAR_MODELS, RUNTIME_INSTALLED};
    let mut first_reason = String::new();
    let models: Vec<serde_json::Value> = DIAR_MODELS
        .iter()
        .map(|model| {
            let reason = verified_path(cache, model).err().unwrap_or_default();
            if first_reason.is_empty() {
                first_reason.clone_from(&reason);
            }
            serde_json::json!({
                "id": model.id, "label": model.label, "repo": model.repo,
                "revision": model.revision, "filename": model.filename,
                "size_bytes": model.size_bytes, "licence": model.licence,
                "ready": reason.is_empty(), "reason": reason,
            })
        })
        .collect();
    let mut status = serde_json::json!({
        "models_ready": models.iter().all(|m| m["ready"] == true),
        "reason": first_reason,
        "extra_installed": RUNTIME_INSTALLED,
        "models": models,
    });
    for name in FLAG_NAMES {
        status[name] = serde_json::json!(config.field_bool(name) == Some(true));
    }
    status
}

/// The Hugging Face hub cache by `huggingface_hub`'s variable precedence, for
/// hosts that don't pass one (`lf_stt::model_storage::hf_hub_cache` is the
/// full version, which also expands `$VARS`).
pub fn default_hub_cache() -> PathBuf {
    let get = |key| std::env::var(key).ok().filter(|v| !v.is_empty());
    let expand = |v: String| lf_core::home::expand_user(Path::new(&v)).unwrap_or_else(|_| v.into());
    if let Some(cache) = get("HF_HUB_CACHE").or_else(|| get("HUGGINGFACE_HUB_CACHE")) {
        return expand(cache);
    }
    if let Some(home) = get("HF_HOME") {
        return expand(home).join("hub");
    }
    let xdg = get("XDG_CACHE_HOME").map(expand).unwrap_or_else(|| {
        lf_core::home::python_home()
            .unwrap_or_else(|_| PathBuf::from("~"))
            .join(".cache")
    });
    xdg.join("huggingface").join("hub")
}

/// `~/.localflow/speakers` (the voiceprint folder), resolved at call time.
pub fn default_speakers_root() -> PathBuf {
    lf_core::home::localflow_dir().join("speakers")
}

/// Python `round(x, ndigits)`: correctly rounded, ties to even.
pub fn py_round(value: f64, digits: usize) -> f64 {
    format!("{value:.digits$}").parse().unwrap_or(value)
}

/// 8 random characters of `[a-z0-9_]`, like `tempfile`'s names.
pub(crate) fn random_token() -> String {
    lf_core::compact_uuid_v4()[..8].to_string()
}

/// `mkdir -p` then 0700 (unix).
pub(crate) fn private_dir(root: &Path) -> io::Result<()> {
    fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// A new file opened exclusively with mode 0600 (unix).
pub(crate) fn create_private(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Write `root/name` through a 0600 temp file `.<prefix>.<random>.tmp` in a
/// 0700 `root`, fsync, then rename; the temp file never outlives a failure.
pub(crate) fn atomic_write(
    root: &Path,
    name: &str,
    prefix: &str,
    data: &[u8],
) -> io::Result<PathBuf> {
    private_dir(root)?;
    let target = root.join(name);
    let tmp = root.join(format!(".{prefix}.{}.tmp", random_token()));
    let result = (|| {
        let mut file = create_private(&tmp)?;
        file.write_all(data)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map(|()| target)
}

/// A regular file, not following symlinks.
pub(crate) fn is_plain_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}
