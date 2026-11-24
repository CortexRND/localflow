//! Pick up config edits without a restart (`localflow/live_config.py`).
//!
//! `ConfigWatcher` waits on filesystem notifications for the config's
//! directory (FSEvents/inotify/ReadDirectoryChangesW via `notify`) instead
//! of a stat timer, so it costs nothing while the file is unchanged.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lf_core::config::Config;

use crate::settings::{DICTATION_ENABLED, DICTATION_TOGGLE_HOTKEY};

/// The Settings agent's ALLOWLIST plus the dictation switch fields.
pub const LIVE_FIELDS: &[&str] = &[
    "sounds_enabled",
    "cleanup_enabled",
    "cleanup_level",
    "styles_enabled",
    "style_work",
    "style_email",
    "style_personal",
    "style_other",
    "hotkey",
    "command_hotkey",
    "polish_hotkey",
    "prompt_hotkey",
    "paste_last_hotkey",
    "paste_method",
    "spoken_symbols",
    "spoken_punctuation",
    "hot_phrases",
    "meeting_watch",
    "meeting_auto_start",
    "capture_system_audio",
    "meeting_default_template",
    "work_prompts",
    DICTATION_ENABLED,
    DICTATION_TOGGLE_HOTKEY,
];

pub const HOTKEY_FIELDS: &[&str] = &[
    "hotkey",
    "command_hotkey",
    "polish_hotkey",
    "prompt_hotkey",
    "paste_last_hotkey",
    DICTATION_TOGGLE_HOTKEY,
];

pub const LLM_FIELDS: &[&str] = &[
    "llm_provider",
    "llm_base_url",
    "llm_model",
    "llm_fast_model",
    "llm_key_ref",
    "cleanup_timeout",
    "cleanup_num_ctx",
];
pub const STT_FIELDS: &[&str] = &["stt_backend", "model_size", "language", "stt_device"];
pub const RUNTIME_GROUPS: &[(&str, &[&str])] = &[("llm", LLM_FIELDS), ("stt", STT_FIELDS)];

/// Fallback poll interval if the platform watcher can't be created.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(2);

pub fn runtime_fields() -> BTreeSet<&'static str> {
    LLM_FIELDS.iter().chain(STT_FIELDS).copied().collect()
}

fn differs(a: &Config, b: &Config, name: &str) -> bool {
    a.field(name) != b.field(name)
}

/// `running` with the live fields taken from `fresh`; also the changed names.
pub fn merge_live(
    running: &Config,
    fresh: &Config,
    reloaded: &BTreeSet<&str>,
) -> (Config, BTreeSet<String>) {
    let changed: BTreeSet<String> = LIVE_FIELDS
        .iter()
        .filter(|n| differs(running, fresh, n))
        .map(|n| n.to_string())
        .collect();
    let others: Vec<&String> = running
        .fields
        .keys()
        .chain(fresh.fields.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|n| {
            !LIVE_FIELDS.contains(&n.as_str())
                && !reloaded.contains(n.as_str())
                && differs(running, fresh, n)
        })
        .collect();
    if !others.is_empty() {
        let names: Vec<&str> = others.iter().map(|s| s.as_str()).collect();
        log::info!(
            "config changed; restart LocalFlow to apply: {}",
            names.join(", ")
        );
    }
    if changed.is_empty() {
        return (running.clone(), changed);
    }
    let mut merged = running.clone();
    for name in &changed {
        match fresh.field(name) {
            Some(v) => merged.set_field(name, v.clone()),
            None => {
                merged.fields.remove(name);
            }
        }
    }
    (merged, changed)
}

/// Names of the RUNTIME_GROUPS whose fields differ.
pub fn runtime_changes(running: &Config, fresh: &Config) -> BTreeSet<&'static str> {
    RUNTIME_GROUPS
        .iter()
        .filter(|(_, fields)| fields.iter().any(|n| differs(running, fresh, n)))
        .map(|(g, _)| *g)
        .collect()
}

/// `running` with each named group copied whole from `fresh`.
pub fn adopt_groups(running: &Config, fresh: &Config, groups: &BTreeSet<&str>) -> Config {
    let mut merged = running.clone();
    for (group, fields) in RUNTIME_GROUPS {
        if groups.contains(group) {
            for name in *fields {
                if let Some(v) = fresh.field(name) {
                    merged.set_field(name, v.clone());
                }
            }
        }
    }
    merged
}

pub type Signature = (u128, u64, u64);

pub fn file_signature(path: &Path) -> Option<Signature> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let ino = 0u64;
    Some((mtime, meta.len(), ino))
}

pub type Loader = Arc<dyn Fn() -> anyhow::Result<Config> + Send + Sync>;

/// A config that refreshes its live fields when read after a file change.
pub struct LazyConfig {
    config: Config,
    path: PathBuf,
    loader: Loader,
    signature: Option<Signature>,
}

impl LazyConfig {
    pub fn new(config: Config, path: PathBuf, loader: Loader) -> Self {
        let signature = file_signature(&path);
        LazyConfig {
            config,
            path,
            loader,
            signature,
        }
    }

    pub fn get(&mut self) -> &Config {
        let signature = file_signature(&self.path);
        if signature != self.signature {
            self.signature = signature;
            match (self.loader)() {
                Ok(fresh) => self.config = merge_live(&self.config, &fresh, &BTreeSet::new()).0,
                Err(e) => log::warn!("config reload skipped: {e}"),
            }
        }
        &self.config
    }
}

pub type OnChange = Arc<dyn Fn(Config) + Send + Sync>;
/// Retry deferred work (a pending shortcut rebind); true while still pending.
pub type OnTick = Arc<dyn Fn() -> bool + Send + Sync>;

struct WatchState {
    signature: Option<Signature>,
}

/// Calls `on_change(config)` after the config file changes on disk.
#[derive(Clone)]
pub struct ConfigWatcher {
    path: PathBuf,
    loader: Loader,
    on_change: OnChange,
    on_tick: Option<OnTick>,
    state: Arc<Mutex<WatchState>>,
    stopped: Arc<AtomicBool>,
    interval: Duration,
}

pub struct WatcherGuard {
    stopped: Arc<AtomicBool>,
    wake: mpsc::Sender<()>,
    _watcher: Option<notify::RecommendedWatcher>,
}

impl Drop for WatcherGuard {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _ = self.wake.send(());
    }
}

impl ConfigWatcher {
    pub fn new(path: PathBuf, loader: Loader, on_change: OnChange) -> Self {
        let signature = file_signature(&path);
        ConfigWatcher {
            path,
            loader,
            on_change,
            on_tick: None,
            state: Arc::new(Mutex::new(WatchState { signature })),
            stopped: Arc::new(AtomicBool::new(false)),
            interval: DEFAULT_INTERVAL,
        }
    }

    pub fn with_tick(mut self, on_tick: OnTick, interval: Duration) -> Self {
        self.on_tick = Some(on_tick);
        self.interval = interval;
        self
    }

    /// Reload and notify if the file changed. True when notified.
    pub fn check(&self) -> bool {
        let signature = file_signature(&self.path);
        {
            let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if signature == st.signature {
                return false;
            }
            st.signature = signature;
        }
        let config = match (self.loader)() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("config reload skipped: {e}");
                return false;
            }
        };
        let notify = self.on_change.clone();
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notify(config))).is_err() {
            log::error!("applying the reloaded config failed");
            return false;
        }
        true
    }

    /// Watch on a background thread. Blocks on file events; wakes on a
    /// timer only while `on_tick` reports deferred work.
    pub fn start(&self) -> WatcherGuard {
        use notify::Watcher;
        let (tx, rx) = mpsc::channel::<()>();
        let events = tx.clone();
        let name = self.path.file_name().map(|n| n.to_os_string());
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                let ours = event
                    .paths
                    .iter()
                    .any(|p| p.file_name().map(|n| n.to_os_string()) == name);
                if ours {
                    let _ = events.send(());
                }
            }
        })
        .and_then(|mut w| {
            let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(dir).map_err(notify::Error::io)?;
            w.watch(dir, notify::RecursiveMode::NonRecursive)?;
            Ok(w)
        });
        let polling = match &watcher {
            Ok(_) => false,
            Err(e) => {
                log::warn!(
                    "file watcher unavailable ({e}); checking the config every {:?}",
                    self.interval
                );
                true
            }
        };
        let this = self.clone();
        std::thread::Builder::new()
            .name("localflow-config-watcher".into())
            .spawn(move || loop {
                let pending = this.on_tick.as_ref().is_some_and(|t| t());
                let woke = if polling || pending {
                    rx.recv_timeout(this.interval)
                        .map_err(|e| matches!(e, mpsc::RecvTimeoutError::Disconnected))
                } else {
                    rx.recv().map_err(|_| true)
                };
                if this.stopped.load(Ordering::SeqCst) || woke == Err(true) {
                    return;
                }
                // Let an editor's write/rename burst settle, then drain it.
                if woke.is_ok() {
                    std::thread::sleep(Duration::from_millis(50));
                    while rx.try_recv().is_ok() {}
                }
                this.check();
            })
            .expect("spawn config watcher");
        WatcherGuard {
            stopped: self.stopped.clone(),
            wake: tx,
            _watcher: watcher.ok(),
        }
    }
}
