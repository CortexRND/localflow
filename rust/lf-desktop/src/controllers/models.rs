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
            ProviderRow {
                id: preset.id,
                label: preset.label,
                base_url: preset.base_url,
                needs_key: preset.needs_key,
                has_key,
                active: models_layout::is_active(preset, config, None),
                status,
                remote: preset.needs_key,
                env_override: (preset.needs_key && env_set(&key_env_var(preset.id)))
                    .then(|| key_env_var(preset.id)),
            }
        })
        .collect()
}

pub fn save_key(
    secrets: &dyn SecretStore,
    file: &ConfigFile,
    provider: &str,
    key: &str,
) -> Result<()> {
    ProviderKeys::new(secrets, file).save_key(provider, key)
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
}
