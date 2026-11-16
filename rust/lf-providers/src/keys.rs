//! Port of `localflow/provider_keys.py`: provider key discovery and
//! keyring-backed setup, independent of any UI.
//!
//! Activation never rewrites a key some running reader may already use: it
//! writes a new endpoint-bound snapshot (see [`crate::activation`]) and then
//! saves the config that points at it in one atomic replace. A failed save
//! deletes only that new snapshot. The generic `llm_api_key` is left to
//! legacy configs. Activation never modifies an existing snapshot.

use crate::activation;
use crate::base::{LlmProvider, ModelInfo};
use crate::ollama::OllamaLlm;
use crate::openai::OpenAiCompatLlm;
use crate::pyfmt::{py_splitlines, py_strip};
use crate::store::{ConfigStore, CoreConfigStore, CoreSecrets, LlmConfig, SecretStore};
use regex::Regex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::OnceLock;

/// A Python `ValueError` from this module (bad input, refused before any
/// write). Keyring and config errors pass through unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidRequest(pub String);

impl fmt::Display for InvalidRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InvalidRequest {}

fn invalid(message: &str) -> anyhow::Error {
    anyhow::Error::new(InvalidRequest(message.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub base_url: &'static str,
    pub env_vars: &'static [&'static str],
    pub llm_provider: &'static str,
    pub needs_key: bool,
    pub default_model: &'static str,
}

const fn cloud(
    id: &'static str,
    label: &'static str,
    base_url: &'static str,
    env_vars: &'static [&'static str],
) -> Preset {
    Preset {
        id,
        label,
        base_url,
        env_vars,
        llm_provider: "openai-compatible",
        needs_key: true,
        default_model: "",
    }
}

pub const PRESETS: [Preset; 7] = [
    Preset {
        id: "ollama",
        label: "Local (Ollama)",
        base_url: "http://localhost:11434",
        env_vars: &[],
        llm_provider: "ollama",
        needs_key: false,
        default_model: "llama3.2:3b",
    },
    cloud(
        "fireworks",
        "Fireworks",
        "https://api.fireworks.ai/inference/v1",
        &["FIREWORKS_API_KEY"],
    ),
    cloud(
        "openai",
        "OpenAI",
        "https://api.openai.com/v1",
        &["OPENAI_API_KEY"],
    ),
    cloud(
        "openrouter",
        "OpenRouter",
        "https://openrouter.ai/api/v1",
        &["OPENROUTER_API_KEY"],
    ),
    cloud(
        "groq",
        "Groq",
        "https://api.groq.com/openai/v1",
        &["GROQ_API_KEY"],
    ),
    cloud(
        "together",
        "Together",
        "https://api.together.xyz/v1",
        &["TOGETHER_API_KEY"],
    ),
    cloud(
        "custom",
        "Custom OpenAI-compatible",
        "",
        &["OPENAI_COMPAT_API_KEY", "LLM_API_KEY"],
    ),
];

/// The default Fireworks model used by work prompts (`Config.fireworks_model`).
pub const FIREWORKS_DEFAULT_MODEL: &str = "accounts/fireworks/models/kimi-k3";

/// `PROVIDER_SECRET_NAMES`: `provider_key_<id>` for every keyed preset.
pub fn provider_secret_names() -> Vec<String> {
    PRESETS
        .iter()
        .filter(|p| p.needs_key)
        .map(secret_name)
        .collect()
}

fn secret_name(preset: &Preset) -> String {
    format!("provider_key_{}", preset.id)
}

pub fn get_preset(provider_id: &str) -> anyhow::Result<&'static Preset> {
    PRESETS
        .iter()
        .find(|p| p.id == provider_id)
        .ok_or_else(|| invalid("Unknown inference provider"))
}

/// "••••" plus the last four characters (only when longer than four).
pub fn mask_key(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = value.chars().collect();
    let tail: String = if chars.len() > 4 {
        chars[chars.len() - 4..].iter().collect()
    } else {
        String::new()
    };
    format!("••••{tail}")
}

/// A discovered key. The value is private and never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct ImportCandidate {
    pub provider_id: String,
    pub source: String,
    value: String,
}

impl ImportCandidate {
    pub fn masked_value(&self) -> String {
        mask_key(&self.value)
    }
}

impl fmt::Debug for ImportCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImportCandidate")
            .field("provider_id", &self.provider_id)
            .field("source", &self.source)
            .finish()
    }
}

fn assignment() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"\A[\s\x1c-\x1f]*(?:export[\s\x1c-\x1f]+)?([A-Za-z_][A-Za-z0-9_]*)[\s\x1c-\x1f]*=[\s\x1c-\x1f]*(.*)\z")
            .unwrap()
    })
}

/// Read literal assignments only; never execute shell code or expand variables.
pub fn parse_env(text: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for line in py_splitlines(text) {
        let Some(caps) = assignment().captures(line) else {
            continue;
        };
        let (name, raw) = (&caps[1], &caps[2]);
        let Ok(parts) = crate::shlex::split(raw) else {
            continue;
        };
        if parts.len() == 1 && !parts[0].is_empty() && !parts[0].contains(['$', '`', ';']) {
            values.insert(name.to_string(), parts[0].clone());
        }
    }
    values
}

/// Keys from `environ` (pass `std::env::vars().collect()` for the process
/// environment) and an optional `.env`/shell profile. Reading the file can
/// fail (missing, not UTF-8): that error is returned.
pub fn detect_imports(
    file: Option<&Path>,
    environ: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<ImportCandidate>> {
    let mut sources: Vec<(String, BTreeMap<String, String>)> =
        vec![("environment".into(), environ.clone())];
    if let Some(file) = file {
        let path = lf_core::home::expand_user(file)?;
        let text = std::fs::read_to_string(&path)?;
        sources.push((path.to_string_lossy().into_owned(), parse_env(&text)));
    }
    let mut found = Vec::new();
    for (source, values) in &sources {
        for preset in PRESETS.iter() {
            for name in preset.env_vars {
                let Some(value) = values.get(*name) else {
                    continue;
                };
                if py_strip(value).is_empty() || value.contains('$') {
                    continue;
                }
                found.push(ImportCandidate {
                    provider_id: preset.id.to_string(),
                    source: format!("{source}: {name}"),
                    value: py_strip(value).to_string(),
                });
            }
        }
    }
    Ok(found)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyStatus {
    pub has_key: bool,
    pub masked_value: String,
}

pub const TEST_OK: &str = "Provider connection successful";
pub const TEST_FAILED: &str = "Provider connection failed; check the key, base URL, and network";
pub const TEST_UNABLE: &str = "Unable to test provider; check the saved key and base URL";
const PROBE_TIMEOUT: i64 = 10;

/// Validated, normalized base URL for a preset (`_url`).
pub fn preset_url(preset: &Preset, base_url: Option<&str>) -> anyhow::Result<String> {
    let url = py_strip(base_url.unwrap_or(preset.base_url))
        .trim_end_matches('/')
        .to_string();
    let parsed = crate::urlcheck::urlsplit(&url).map_err(invalid)?;
    let truthy = |v: &Option<String>| v.as_deref().is_some_and(|s| !s.is_empty());
    if !matches!(parsed.scheme.as_str(), "http" | "https")
        || !truthy(&parsed.hostname)
        || truthy(&parsed.username)
        || truthy(&parsed.password)
        || !parsed.query.is_empty()
        || !parsed.fragment.is_empty()
    {
        return Err(invalid(
            "Use an HTTP(S) base URL without credentials, query, or fragment",
        ));
    }
    if preset.id != "custom" && url != preset.base_url {
        return Err(invalid("Use the custom preset for a different base URL"));
    }
    Ok(url)
}

/// The stripped model to activate (`_choose_model`): an explicit `model`
/// wins, `None` keeps `current`; blank falls back to the preset default
/// (only Ollama has one); otherwise the caller must choose.
pub fn choose_model(preset: &Preset, model: Option<&str>, current: &str) -> anyhow::Result<String> {
    let mut picked = py_strip(model.unwrap_or(current)).to_string();
    if picked.is_empty() {
        picked = py_strip(preset.default_model).to_string();
    }
    if picked.is_empty() {
        return Err(invalid("Choose a model before using this provider"));
    }
    Ok(picked)
}

/// Provider key operations against an injected keyring and config store.
pub struct ProviderKeys<'a, C> {
    pub secrets: &'a dyn SecretStore,
    pub configs: &'a dyn ConfigStore<C>,
}

impl ProviderKeys<'static, lf_core::config::Config> {
    /// The real keyring (via `lf_core::secrets`) and config file.
    pub fn system() -> Self {
        static SECRETS: CoreSecrets = CoreSecrets;
        static CONFIGS: CoreConfigStore = CoreConfigStore;
        ProviderKeys {
            secrets: &SECRETS,
            configs: &CONFIGS,
        }
    }
}

impl<'a, C: LlmConfig> ProviderKeys<'a, C> {
    pub fn new(secrets: &'a dyn SecretStore, configs: &'a dyn ConfigStore<C>) -> Self {
        ProviderKeys { secrets, configs }
    }

    pub fn save_key(&self, provider_id: &str, value: &str) -> anyhow::Result<()> {
        let preset = get_preset(provider_id)?;
        let value = py_strip(value);
        if !preset.needs_key || value.is_empty() {
            return Err(invalid("A non-empty key and a keyed provider are required"));
        }
        self.secrets.set(&secret_name(preset), value)?;
        if preset.id == "fireworks" {
            self.secrets.set("fireworks_api_key", value)?;
        }
        Ok(())
    }

    pub fn save_imports(&self, confirmed: &[ImportCandidate]) -> anyhow::Result<()> {
        for candidate in confirmed {
            self.save_key(&candidate.provider_id, &candidate.value)?;
        }
        Ok(())
    }

    fn write_index(&self, refs: &[String]) -> anyhow::Result<()> {
        if !refs.is_empty() {
            self.secrets.set(
                activation::INDEX_NAME,
                &activation::encode_index(refs.iter().map(String::as_str)),
            )
        } else if !self.secrets.get(activation::INDEX_NAME).is_empty() {
            self.secrets.delete(activation::INDEX_NAME)
        } else {
            Ok(())
        }
    }

    fn read_index(&self) -> Vec<String> {
        activation::read_index(|name| self.secrets.get(name))
    }

    /// Delete every activation snapshot taken from `provider_id`'s key.
    fn revoke_activations(&self, provider_id: &str, config: Option<&C>) -> anyhow::Result<()> {
        let index = self.read_index();
        let mut revoke: Vec<String> = index
            .iter()
            .filter(|r| activation::ref_provider(r) == Some(provider_id))
            .cloned()
            .collect();
        let current = config.map(|c| c.get_str("llm_key_ref")).unwrap_or_default();
        if activation::ref_provider(&current) == Some(provider_id) && !revoke.contains(&current) {
            revoke.push(current);
        }
        for reference in &revoke {
            if !self.secrets.get(reference).is_empty() {
                self.secrets.delete(reference)?;
            }
        }
        let remaining: Vec<String> = index
            .iter()
            .filter(|r| !revoke.contains(r))
            .cloned()
            .collect();
        if remaining != index {
            self.write_index(&remaining)?;
        }
        Ok(())
    }

    /// Remove the saved key and revoke its activations (the active key too).
    /// Revokes first: if the keyring refuses, the saved key is still there.
    pub fn remove_key(&self, provider_id: &str, config: Option<&C>) -> anyhow::Result<()> {
        let preset = get_preset(provider_id)?;
        if !preset.needs_key {
            return Ok(());
        }
        self.revoke_activations(preset.id, config)?;
        self.secrets.delete(&secret_name(preset))?;
        if preset.id == "fireworks" {
            self.secrets.delete("fireworks_api_key")?;
        }
        Ok(())
    }

    /// The key the active config really sends (same rule as `build_llm`).
    fn active_key(&self, config: &C) -> String {
        crate::factory::llm_api_key(config, self.secrets)
    }

    /// True when a saved key differs from the key Ask and cleanup actually
    /// use for `config` (loaded when None). Values never leave this function.
    pub fn key_needs_activation(
        &self,
        provider_id: &str,
        config: Option<&C>,
    ) -> anyhow::Result<bool> {
        let preset = get_preset(provider_id)?;
        if !preset.needs_key {
            return Ok(false);
        }
        let saved = self.secrets.get(&secret_name(preset));
        if saved.is_empty() {
            return Ok(false);
        }
        let active = match config {
            Some(config) => self.active_key(config),
            None => self.active_key(&self.configs.load()?),
        };
        Ok(saved != active)
    }

    pub fn key_status(&self, provider_id: &str) -> anyhow::Result<KeyStatus> {
        let preset = get_preset(provider_id)?;
        let value = if preset.needs_key {
            self.secrets.get(&secret_name(preset))
        } else {
            String::new()
        };
        Ok(KeyStatus {
            has_key: !value.is_empty(),
            masked_value: mask_key(&value),
        })
    }

    /// Make `provider_id` the one Ask and cleanup use; atomic for readers.
    ///
    /// Keyed providers: (1) record a fresh snapshot name in the activation
    /// index, (2) write the immutable snapshot {endpoint, key}, (3) atomically
    /// save the config naming it. On failure only what this call added is
    /// removed again, and the original error is returned.
    pub fn activate(
        &self,
        provider_id: &str,
        base_url: Option<&str>,
        model: Option<&str>,
        config: Option<C>,
    ) -> anyhow::Result<C> {
        let preset = get_preset(provider_id)?;
        let url = preset_url(preset, base_url)?;
        let base = match config {
            Some(config) => config,
            None => self.configs.load()?,
        };
        // Decided before any key or config write: never persist an unusable model.
        let chosen = choose_model(preset, model, &base.get_str("llm_model"))?;
        let key = if preset.needs_key {
            self.secrets.get(&secret_name(preset))
        } else {
            String::new()
        };
        if preset.needs_key && key.is_empty() {
            return Err(invalid("Save a provider key before activating it"));
        }
        let mut updated = base.clone();
        updated.set_str("llm_provider", preset.llm_provider);
        updated.set_str("llm_base_url", &url);
        updated.set_str("llm_key_ref", "");
        updated.set_str("llm_model", &chosen);
        if !preset.needs_key {
            self.configs.save(&updated)?; // keyless (Ollama): nothing to pair
            return Ok(updated);
        }
        let reused = self.reusable_snapshot(&base, preset.id, &url, &key);
        if !reused.is_empty() {
            updated.set_str("llm_key_ref", &reused);
            self.configs.save(&updated)?; // the existing snapshot is untouched either way
            return Ok(updated);
        }
        let index = self.read_index();
        let reference = self.unused_ref(preset.id)?;
        let mut with_new = index.clone();
        with_new.push(reference.clone());
        self.write_index(&with_new)?; // fails before anything a reader could see
        let result = self
            .secrets
            .set(&reference, &activation::encode(&url, &key))
            .and_then(|()| {
                updated.set_str("llm_key_ref", &reference);
                self.configs.save(&updated)
            });
        if let Err(err) = result {
            self.discard_snapshot(&reference, &index);
            return Err(err);
        }
        Ok(updated)
    }

    /// Re-activating the same key for the same endpoint reuses its snapshot.
    fn reusable_snapshot(&self, config: &C, provider_id: &str, url: &str, key: &str) -> String {
        let reference = config.get_str("llm_key_ref");
        if activation::ref_provider(&reference) != Some(provider_id) {
            return String::new();
        }
        if activation::decode(&self.secrets.get(&reference))
            != Some((activation::endpoint(url), key.to_string()))
        {
            return String::new();
        }
        reference
    }

    fn unused_ref(&self, provider_id: &str) -> anyhow::Result<String> {
        for _ in 0..3 {
            let reference = activation::new_ref(provider_id)?;
            if self.secrets.get(&reference).is_empty() {
                return Ok(reference);
            }
        }
        anyhow::bail!("Could not allocate a provider activation")
    }

    /// Undo a failed activation: delete only the snapshot it created. Never
    /// fails and never logs a key; if the delete fails, the orphan stays in
    /// the index so removing the provider key still revokes it.
    fn discard_snapshot(&self, reference: &str, index: &[String]) {
        let deleted = if self.secrets.get(reference).is_empty() {
            Ok(())
        } else {
            self.secrets.delete(reference)
        };
        if deleted.is_err() {
            log::warn!(
                "activation failed and its unused key snapshot could not be removed; \
                 no config refers to it, and removing the provider key deletes it"
            );
            return;
        }
        if self.write_index(index).is_err() {
            log::warn!("could not tidy the provider activation index");
        }
    }

    /// A probe client for a preset with its own saved key (`_provider`).
    fn probe(
        &self,
        provider_id: &str,
        base_url: Option<&str>,
        model: &str,
    ) -> anyhow::Result<Box<dyn LlmProvider>> {
        let preset = get_preset(provider_id)?;
        let url = preset_url(preset, base_url)?;
        if !preset.needs_key {
            return Ok(Box::new(OllamaLlm::new(
                &url,
                model,
                PROBE_TIMEOUT,
                crate::ollama::DEFAULT_NUM_CTX,
            )));
        }
        let key = self.secrets.get(&secret_name(preset));
        if key.is_empty() {
            return Err(invalid("Save a provider key first"));
        }
        Ok(Box::new(OpenAiCompatLlm::new(
            &url,
            model,
            &key,
            PROBE_TIMEOUT,
            None,
        )))
    }

    /// Models for a preset using its saved key; any failure yields [].
    pub fn fetch_models(&self, provider_id: &str, base_url: Option<&str>) -> Vec<ModelInfo> {
        self.probe(provider_id, base_url, "")
            .ok()
            .and_then(|provider| provider.list_models().ok())
            .unwrap_or_default()
    }

    /// (ok, fixed message); provider details are never surfaced.
    pub fn test_llm(
        &self,
        provider_id: &str,
        base_url: Option<&str>,
        model: &str,
    ) -> (bool, String) {
        match self.probe(provider_id, base_url, model) {
            Ok(provider) if provider.healthcheck().ok => (true, TEST_OK.into()),
            Ok(_) => (false, TEST_FAILED.into()),
            Err(_) => (false, TEST_UNABLE.into()),
        }
    }
}

#[cfg(test)]
mod tests;
