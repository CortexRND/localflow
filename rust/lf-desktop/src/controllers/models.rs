//! Models & keys: speech model, cleanup model and provider keys, through the
//! portable rules in `lf_ui::models_layout` and `lf_providers::keys`.
//!
//! Key values go straight from the secret field to the secret store; they are
//! never kept in page state after saving, never shown and never logged.

use anyhow::Result;
use lf_core::config::Config;
use lf_providers::keys::Preset;
use lf_providers::{ProviderKeys, SecretStore, PRESETS};
use lf_ui::models_layout;

use crate::data::ConfigFile;

/// Whether this machine can run a speech provider. Only MLX is known to be
/// impossible here; the rest is confirmed when the speech engine loads.
pub fn speech_available(provider: &str) -> bool {
    !matches!(provider, "mlx-whisper" | "mlx")
        || cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

pub fn speech_unavailable_reason(provider: &str) -> Option<&'static str> {
    (!speech_available(provider)).then_some("MLX Whisper needs a Mac with Apple silicon.")
}

/// Save the speech provider/model (validated by `models_layout::set_speech`).
pub fn set_speech(file: &ConfigFile, provider: &str, model: Option<&str>) -> Result<Config> {
    file.update(|latest| {
        models_layout::set_speech(
            provider,
            model,
            latest,
            &speech_available,
            &|_| Ok(()),
            &|_| {},
        )
    })
}

/// Save the cleanup model of the active provider.
pub fn set_cleanup_model(file: &ConfigFile, model: &str) -> Result<Config> {
    file.update(|latest| models_layout::set_active_model(model, latest, None, &|_| Ok(())))
}

/// A provider row: never carries the key itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRow {
    pub id: &'static str,
    pub label: &'static str,
    pub base_url: &'static str,
    pub needs_key: bool,
    pub has_key: bool,
    pub active: bool,
    pub status: &'static str,
    pub remote: bool,
    /// The environment variable that overrides the keyring for this
    /// provider, when it is set (`LOCALFLOW_PROVIDER_KEY_<ID>`). Saving or
    /// removing a keyring entry would have no effect while it is set.
    pub env_override: Option<String>,
    /// Active provider whose saved key is newer than the key cleanup and Ask
    /// actually use (activation snapshots are immutable): Use must stay
    /// available to switch to it.
    pub needs_reactivation: bool,
}

/// `LOCALFLOW_PROVIDER_KEY_<ID>`: the variable `lf_core::secrets` reads first.
pub fn key_env_var(provider_id: &str) -> String {
    lf_core::secrets::env_name(&format!("provider_key_{provider_id}"))
}

/// `env_set` reports whether an environment variable is set; pass
/// `&|_| false` where the environment is not consulted (sandbox).
pub fn provider_rows(
    secrets: &dyn SecretStore,
    file: &ConfigFile,
    config: &Config,
    env_set: &dyn Fn(&str) -> bool,
) -> Vec<ProviderRow> {
    let keys = ProviderKeys::new(secrets, file);
    PRESETS
        .iter()
        .map(|preset: &Preset| {
            let has_key = keys
                .key_status(preset.id)
                .map(|s| s.has_key)
                .unwrap_or(false);
            let (status, _) = models_layout::provider_status(preset, config, has_key);
            let active = models_layout::is_active(preset, config, None);
            let needs_reactivation = active
                && keys
                    .key_needs_activation(preset.id, Some(config))
                    .unwrap_or(false);
            ProviderRow {
                id: preset.id,
                label: preset.label,
                base_url: preset.base_url,
                needs_key: preset.needs_key,
                has_key,
                active,
                needs_reactivation,
                status,
                remote: preset.needs_key,
                env_override: (preset.needs_key && env_set(&key_env_var(preset.id)))
                    .then(|| key_env_var(preset.id)),
            }
        })
        .collect()
}

/// Provider rows read once (keyring + environment) off the UI thread, so
/// rendering never touches the keyring. Holds no key values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderSnapshot {
    pub rows: Vec<ProviderRow>,
}

impl ProviderSnapshot {
    /// Blocking: reads key presence from `secrets`. Call on a worker.
    pub fn load(
        secrets: &dyn SecretStore,
        file: &ConfigFile,
        config: &Config,
        env_set: &dyn Fn(&str) -> bool,
    ) -> Self {
        ProviderSnapshot {
            rows: provider_rows(secrets, file, config, env_set),
        }
    }

    pub fn row(&self, id: &str) -> Option<&ProviderRow> {
        self.rows.iter().find(|r| r.id == id)
    }

    pub fn has_key(&self, id: &str) -> bool {
        self.row(id).is_some_and(|r| r.has_key)
    }

    pub fn env_override(&self, id: &str) -> Option<String> {
        self.row(id).and_then(|r| r.env_override.clone())
    }
}

pub fn save_key(
    secrets: &dyn SecretStore,
    file: &ConfigFile,
    provider: &str,
    key: &str,
) -> Result<()> {
    ProviderKeys::new(secrets, file).save_key(provider, key)
}

/// What happened to a saved key.
#[derive(Clone, Debug, PartialEq)]
pub enum KeySaved {
    /// Saved; the provider isn't the active one, so nothing else changed.
    Saved,
    /// Saved, and the active provider was re-activated with it (endpoint and
    /// model kept), so cleanup and Ask use the new key now.
    Activated(Config),
    /// Saved, but switching the active provider to it failed; the previous
    /// key stays in use. The message says why.
    NotActivated(String),
}

/// Save a key and, when its provider is the active one, switch to it at
/// once: `ProviderKeys::save_key` alone leaves the active (immutable)
/// activation snapshot on the old key.
pub fn save_key_and_apply(
    secrets: &dyn SecretStore,
    file: &ConfigFile,
    provider: &str,
    key: &str,
) -> Result<KeySaved> {
    save_key(secrets, file, provider, key)?;
    // "Is it still the active provider?" and the re-activation are one
    // config transaction: an in-app provider change can't land between them
    // and be switched back by this key save.
    lf_ui::config_write::with_lock(|| reactivate_if_active(secrets, file, provider))
}

fn reactivate_if_active(
    secrets: &dyn SecretStore,
    file: &ConfigFile,
    provider: &str,
) -> Result<KeySaved> {
    let config = file.load()?;
    let preset = lf_providers::keys::get_preset(provider)?;
    if !models_layout::is_active(preset, &config, None) {
        return Ok(KeySaved::Saved);
    }
    let base_url = config.field_str("llm_base_url").unwrap_or("").to_string();
    let model = config.field_str("llm_model").unwrap_or("").to_string();
    let url = (preset.id == "custom").then_some(base_url.as_str());
    Ok(
        match activate(
            secrets,
            file,
            provider,
            url,
            Some(model.as_str()).filter(|m| !m.is_empty()),
        ) {
            Ok(saved) => KeySaved::Activated(saved),
            Err(err) => KeySaved::NotActivated(error_text(&err)),
        },
    )
}

pub fn remove_key(secrets: &dyn SecretStore, file: &ConfigFile, provider: &str) -> Result<()> {
    let config = file.load()?;
    ProviderKeys::new(secrets, file).remove_key(provider, Some(&config))
}

/// Make a provider the one cleanup and Ask use.
pub fn activate(
    secrets: &dyn SecretStore,
    file: &ConfigFile,
    provider: &str,
    base_url: Option<&str>,
    model: Option<&str>,
) -> Result<Config> {
    lf_ui::config_write::with_lock(|| {
        let latest = file.load()?;
        ProviderKeys::new(secrets, file).activate(provider, base_url, model, Some(latest))
    })
}

/// Where cleanup text goes, in words.
pub fn data_leaves_text(config: &Config) -> String {
    match models_layout::active_preset(config) {
        Some(p) if !p.needs_key => "Nothing. Cleanup runs on this device through Ollama.".into(),
        Some(p) => format!("Dictated text is sent to {} for cleanup and Ask.", p.label),
        None => format!(
            "Dictated text is sent to {} for cleanup and Ask.",
            config
                .field_str("llm_base_url")
                .unwrap_or("the configured provider")
        ),
    }
}

/// Friendly text for an error from the key/model operations.
pub fn error_text(error: &anyhow::Error) -> String {
    if let Some(e) = error.downcast_ref::<models_layout::ModelChoiceError>() {
        return e.0.clone();
    }
    if let Some(e) = error.downcast_ref::<lf_providers::keys::InvalidRequest>() {
        return e.0.clone();
    }
    if error
        .downcast_ref::<lf_core::secrets::SecretsUnavailable>()
        .is_some()
    {
        return "Your system keyring isn't available, so the key wasn't saved. You can set the \
                LOCALFLOW_<NAME> environment variable instead."
            .into();
    }
    format!("{error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{DataRoot, MemorySecrets};

    fn setup() -> (tempfile::TempDir, ConfigFile, MemorySecrets) {
        let dir = tempfile::tempdir().unwrap();
        let file = ConfigFile::new(DataRoot::sandbox(dir.path()));
        (dir, file, MemorySecrets::default())
    }

    #[test]
    fn speech_choice_is_validated_and_saved() {
        let (_d, file, _) = setup();
        let saved = set_speech(&file, "faster-whisper", Some("small")).unwrap();
        assert_eq!(saved.field_str("stt_backend"), Some("faster-whisper"));
        assert_eq!(saved.field_str("model_size"), Some("small"));
        assert!(set_speech(&file, "warp-drive", None).is_err());
        if !speech_available("mlx-whisper") {
            assert!(set_speech(&file, "mlx-whisper", Some("base")).is_err());
        }
    }

    #[test]
    fn cleanup_model_ids_are_validated() {
        let (_d, file, _) = setup();
        assert!(set_cleanup_model(&file, "bad model id!").is_err());
        let saved = set_cleanup_model(&file, "qwen3:4b").unwrap();
        assert_eq!(saved.field_str("llm_model"), Some("qwen3:4b"));
    }

    #[test]
    fn keys_save_activate_and_remove_without_exposing_values() {
        let (_d, file, secrets) = setup();
        let config = file.load().unwrap();
        let rows = provider_rows(&secrets, &file, &config, &|_| false);
        let openai = rows.iter().find(|r| r.id == "openai").unwrap();
        assert!(!openai.has_key);
        assert!(activate(&secrets, &file, "openai", None, Some("gpt-5-mini")).is_err());
        save_key(&secrets, &file, "openai", "  sk-test-123  ").unwrap();
        let rows = provider_rows(&secrets, &file, &file.load().unwrap(), &|_| false);
        let openai = rows.iter().find(|r| r.id == "openai").unwrap();
        assert!(openai.has_key);
        assert_eq!(openai.status, "Key saved");
        assert!(!format!("{rows:?}").contains("sk-test"));
        let saved = activate(&secrets, &file, "openai", None, Some("gpt-5-mini")).unwrap();
        assert_eq!(saved.field_str("llm_model"), Some("gpt-5-mini"));
        assert!(data_leaves_text(&saved).contains("OpenAI"));
        let on_disk = std::fs::read_to_string(file.path()).unwrap();
        assert!(
            !on_disk.contains("sk-test"),
            "the key must never reach the config file"
        );
        remove_key(&secrets, &file, "openai").unwrap();
        let rows = provider_rows(&secrets, &file, &file.load().unwrap(), &|_| false);
        assert!(!rows.iter().find(|r| r.id == "openai").unwrap().has_key);
        let back = activate(&secrets, &file, "ollama", None, Some("llama3.2:3b")).unwrap();
        assert!(data_leaves_text(&back).starts_with("Nothing"));
    }

    #[test]
    fn provider_snapshot_reads_the_keyring_once_and_never_while_queried() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Recording {
            inner: MemorySecrets,
            reads: AtomicUsize,
        }
        impl SecretStore for Recording {
            fn get(&self, name: &str) -> String {
                self.reads.fetch_add(1, Ordering::SeqCst);
                self.inner.get(name)
            }
            fn set(&self, name: &str, value: &str) -> Result<()> {
                self.inner.set(name, value)
            }
            fn delete(&self, name: &str) -> Result<()> {
                self.inner.delete(name)
            }
        }
        let (_d, file, inner) = setup();
        inner
            .set("provider_key_groq", "gsk-snapshot-secret")
            .unwrap();
        let secrets = Recording {
            inner,
            reads: AtomicUsize::new(0),
        };
        let config = file.load().unwrap();
        let groq = key_env_var("groq");
        let snapshot = ProviderSnapshot::load(&secrets, &file, &config, &|n| n == groq);
        let after_load = secrets.reads.load(Ordering::SeqCst);
        assert!(after_load > 0, "the load itself reads key presence");
        for _ in 0..50 {
            assert!(snapshot.has_key("groq"));
            assert!(!snapshot.has_key("openai"));
            assert_eq!(
                snapshot.env_override("groq").as_deref(),
                Some("LOCALFLOW_PROVIDER_KEY_GROQ")
            );
        }
        assert_eq!(
            secrets.reads.load(Ordering::SeqCst),
            after_load,
            "queries never hit the keyring"
        );
        assert!(
            !format!("{snapshot:?}").contains("gsk-snapshot-secret"),
            "no values kept"
        );
    }

    #[test]
    fn environment_overrides_are_reported_not_hidden() {
        let (_d, file, secrets) = setup();
        let config = file.load().unwrap();
        let groq = key_env_var("groq");
        assert_eq!(groq, "LOCALFLOW_PROVIDER_KEY_GROQ");
        let rows = provider_rows(&secrets, &file, &config, &|name| name == groq);
        let row = rows.iter().find(|r| r.id == "groq").unwrap();
        assert_eq!(
            row.env_override.as_deref(),
            Some("LOCALFLOW_PROVIDER_KEY_GROQ")
        );
        assert!(rows
            .iter()
            .find(|r| r.id == "ollama")
            .unwrap()
            .env_override
            .is_none());
    }

    /// The key cleanup and Ask really send for `config`.
    fn active_key(secrets: &MemorySecrets, config: &Config) -> String {
        lf_providers::factory::llm_api_key(config, secrets)
    }

    #[test]
    fn replacing_the_active_key_switches_to_it_and_keeps_endpoint_and_model() {
        let (_d, file, secrets) = setup();
        save_key(&secrets, &file, "openai", "sk-old").unwrap();
        let first = activate(&secrets, &file, "openai", None, Some("gpt-5-mini")).unwrap();
        assert_eq!(active_key(&secrets, &first), "sk-old");

        // The old path: saving alone leaves the runtime on the old snapshot,
        // and the row must keep a way to switch.
        save_key(&secrets, &file, "openai", "sk-new").unwrap();
        let config = file.load().unwrap();
        assert_eq!(active_key(&secrets, &config), "sk-old");
        let rows = provider_rows(&secrets, &file, &config, &|_| false);
        let row = rows.iter().find(|r| r.id == "openai").unwrap();
        assert!(row.active && row.needs_reactivation);

        // Saving through the page's path re-activates with the new key.
        let outcome = save_key_and_apply(&secrets, &file, "openai", "sk-newer").unwrap();
        let KeySaved::Activated(saved) = outcome else {
            panic!("expected activation, got {outcome:?}");
        };
        assert_eq!(active_key(&secrets, &saved), "sk-newer");
        assert_eq!(saved.field_str("llm_model"), Some("gpt-5-mini"));
        assert_eq!(
            saved.field_str("llm_base_url"),
            first.field_str("llm_base_url")
        );
        let reloaded = file.load().unwrap();
        assert_eq!(active_key(&secrets, &reloaded), "sk-newer");
        let rows = provider_rows(&secrets, &file, &reloaded, &|_| false);
        assert!(
            !rows
                .iter()
                .find(|r| r.id == "openai")
                .unwrap()
                .needs_reactivation
        );
        assert!(!std::fs::read_to_string(file.path())
            .unwrap()
            .contains("sk-"));

        // A key for a provider that isn't active is only saved.
        assert_eq!(
            save_key_and_apply(&secrets, &file, "groq", "gsk-1").unwrap(),
            KeySaved::Saved
        );
        assert_eq!(active_key(&secrets, &file.load().unwrap()), "sk-newer");
    }

    #[test]
    fn a_key_save_cannot_switch_back_a_provider_changed_in_between() {
        let (_d, file, secrets) = setup();
        let secrets = std::sync::Arc::new(secrets);
        save_key(secrets.as_ref(), &file, "openai", "sk-old").unwrap();
        activate(secrets.as_ref(), &file, "openai", None, Some("gpt-5-mini")).unwrap();
        let (locked, held) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let switcher = {
            let (file, secrets) = (file.clone(), secrets.clone());
            std::thread::spawn(move || {
                lf_ui::config_write::with_lock(|| {
                    locked.send(()).unwrap();
                    released.recv().unwrap();
                    // "Use" Ollama, inside the config transaction.
                    activate(secrets.as_ref(), &file, "ollama", None, Some("qwen3:4b")).unwrap();
                });
            })
        };
        held.recv().unwrap();
        let saver = {
            let (file, secrets) = (file.clone(), secrets.clone());
            std::thread::spawn(move || {
                save_key_and_apply(secrets.as_ref(), &file, "openai", "sk-new")
            })
        };
        release.send(()).unwrap();
        switcher.join().unwrap();
        let outcome = saver.join().unwrap().unwrap();
        assert_eq!(outcome, KeySaved::Saved, "openai isn't active any more");
        let config = file.load().unwrap();
        assert_eq!(config.field_str("llm_provider"), Some("ollama"));
        assert_eq!(config.field_str("llm_model"), Some("qwen3:4b"));
    }

    #[test]
    fn failed_reactivation_keeps_the_old_key_in_use_and_says_so() {
        let (_d, file, secrets) = setup();
        save_key(&secrets, &file, "openai", "sk-old").unwrap();
        activate(&secrets, &file, "openai", None, Some("gpt-5-mini")).unwrap();
        // Make activation impossible: no model to keep (choose_model refuses).
        let mut broken = file.load().unwrap();
        broken.set_field("llm_model", serde_json::json!(""));
        lf_core::config::save_config(&broken, Some(file.path())).unwrap();
        let outcome = save_key_and_apply(&secrets, &file, "openai", "sk-new").unwrap();
        let KeySaved::NotActivated(message) = outcome else {
            panic!("expected a reported failure, got {outcome:?}");
        };
        assert!(!message.is_empty());
        let config = file.load().unwrap();
        assert_eq!(
            active_key(&secrets, &config),
            "sk-old",
            "runtime key unchanged"
        );
        let rows = provider_rows(&secrets, &file, &config, &|_| false);
        let row = rows.iter().find(|r| r.id == "openai").unwrap();
        assert!(
            row.has_key && row.needs_reactivation,
            "the switch stays offered"
        );
    }
}
