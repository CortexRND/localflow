//! Import provider keys from the environment or a `.env` file, and test a
//! provider connection, through `lf_providers::keys`.
//!
//! Key values live only inside `ImportCandidate` (private field, redacting
//! `Debug`) and go straight to `ProviderKeys::save_imports`. Rows, counts,
//! toasts and errors carry provider names and variable names only — never a
//! value, not even masked. Importing saves keys; it never changes which
//! provider cleanup and Ask use.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use lf_providers::keys::{self, ImportCandidate};
use lf_providers::{ConfigStore, ProviderKeys, SecretStore};
use lf_ui::models_layout::{import_states, ImportKey};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Environment,
    File(PathBuf),
}

impl Source {
    pub fn describe(&self) -> String {
        match self {
            Source::Environment => "the environment".into(),
            Source::File(path) => path.display().to_string(),
        }
    }
}

/// One importable key, without its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportRow {
    pub key: ImportKey,
    pub provider_id: String,
    pub provider_label: String,
    /// `environment: OPENAI_API_KEY` or `<path>: GROQ_API_KEY`.
    pub source: String,
    /// The provider already has a saved key that importing replaces.
    pub replaces: bool,
    /// Why this row can't be imported (an environment override would win).
    pub blocked: Option<String>,
    pub selected: bool,
}

pub struct KeyImport {
    pub source: Source,
    candidates: Vec<ImportCandidate>,
    overrides: BTreeMap<ImportKey, bool>,
}

impl std::fmt::Debug for KeyImport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyImport")
            .field("source", &self.source)
            .field("candidates", &self.candidates)
            .finish()
    }
}

fn label(provider_id: &str) -> String {
    keys::get_preset(provider_id)
        .map(|p| p.label.to_string())
        .unwrap_or_else(|_| provider_id.to_string())
}

impl KeyImport {
    /// Scan `environ` (environment source) or the file at `path`. Reading
    /// the file is local; nothing is executed or expanded.
    pub fn detect(source: Source, environ: &BTreeMap<String, String>) -> Result<Self, String> {
        let candidates = match &source {
            Source::Environment => keys::detect_imports(None, environ)
                .map_err(|e| format!("Couldn't read the environment: {e}"))?,
            Source::File(path) => keys::detect_imports(Some(path), &BTreeMap::new())
                .map_err(|e| format!("Couldn't read {}: {}", path.display(), io_reason(&e)))?
                .into_iter()
                .filter(|c| !c.source.starts_with("environment:"))
                .collect(),
        };
        Ok(KeyImport {
            source,
            candidates,
            overrides: BTreeMap::new(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    /// "Found 3 keys for 2 providers." / the nothing-found sentence.
    pub fn summary(&self) -> String {
        if self.candidates.is_empty() {
            return format!(
                "No provider keys found in {}. LocalFlow looks for names such as OPENAI_API_KEY.",
                self.source.describe()
            );
        }
        let providers: std::collections::BTreeSet<&str> = self
            .candidates
            .iter()
            .map(|c| c.provider_id.as_str())
            .collect();
        format!(
            "Found {} key{} for {} provider{}.",
            self.candidates.len(),
            if self.candidates.len() == 1 { "" } else { "s" },
            providers.len(),
            if providers.len() == 1 { "" } else { "s" },
        )
    }

    /// Rows with their tick state: on when the provider has no saved key
    /// (`models_layout::import_states`), user changes win; a provider whose
    /// key comes from a `LOCALFLOW_…` variable can't be imported (the saved
    /// key would never be used). At most one source per provider is ticked.
    pub fn rows(
        &self,
        has_key: &dyn Fn(&str) -> bool,
        env_override: &dyn Fn(&str) -> Option<String>,
    ) -> Vec<ImportRow> {
        let states = import_states(&self.candidates, has_key, &self.overrides);
        let mut taken = std::collections::BTreeSet::new();
        self.candidates
            .iter()
            .map(|c| {
                let key = (c.provider_id.clone(), c.source.clone());
                let blocked = env_override(&c.provider_id).map(|var| {
                    format!("{var} is set in the environment and overrides any saved key, so importing wouldn't change anything.")
                });
                let wanted = states.get(&key).copied().unwrap_or(false) && blocked.is_none();
                let selected = wanted && taken.insert(c.provider_id.clone());
                ImportRow {
                    provider_label: label(&c.provider_id),
                    provider_id: c.provider_id.clone(),
                    source: c.source.clone(),
                    replaces: has_key(&c.provider_id),
                    blocked,
                    selected,
                    key,
                }
            })
            .collect()
    }

    /// Tick or untick a row. Ticking one source of a provider unticks its
    /// other sources.
    pub fn toggle(&mut self, key: &ImportKey, on: bool) {
        if on {
            for c in &self.candidates {
                if c.provider_id == key.0 && c.source != key.1 {
                    self.overrides
                        .insert((c.provider_id.clone(), c.source.clone()), false);
                }
            }
        }
        self.overrides.insert(key.clone(), on);
    }

    /// Save the ticked rows. Returns the provider labels imported.
    pub fn import<C: lf_providers::LlmConfig>(
        &self,
        keys: &ProviderKeys<'_, C>,
        has_key: &dyn Fn(&str) -> bool,
        env_override: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Vec<String>, String> {
        let rows = self.rows(has_key, env_override);
        let confirmed: Vec<ImportCandidate> = self
            .candidates
            .iter()
            .filter(|c| {
                rows.iter()
                    .any(|r| r.selected && r.provider_id == c.provider_id && r.source == c.source)
            })
            .cloned()
            .collect();
        if confirmed.is_empty() {
            return Err("Nothing selected.".into());
        }
        keys.save_imports(&confirmed)
            .map_err(|e| crate::controllers::models::error_text(&e))?;
        Ok(confirmed.iter().map(|c| label(&c.provider_id)).collect())
    }

    pub fn selected_count(
        &self,
        has_key: &dyn Fn(&str) -> bool,
        env_override: &dyn Fn(&str) -> Option<String>,
    ) -> usize {
        self.rows(has_key, env_override)
            .iter()
            .filter(|r| r.selected)
            .count()
    }
}

fn io_reason(error: &anyhow::Error) -> String {
    match error.downcast_ref::<std::io::Error>() {
        Some(io) => match io.kind() {
            std::io::ErrorKind::NotFound => "the file doesn't exist".into(),
            std::io::ErrorKind::PermissionDenied => "permission denied".into(),
            std::io::ErrorKind::InvalidData => "it isn't a text file".into(),
            _ => "it couldn't be read".into(),
        },
        None => "it couldn't be read".into(),
    }
}

/// The result of Test connection: one of the three fixed sentences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestOutcome {
    Ok,
    Failed,
    Unable,
}

impl TestOutcome {
    pub fn text(self) -> &'static str {
        match self {
            TestOutcome::Ok => keys::TEST_OK,
            TestOutcome::Failed => keys::TEST_FAILED,
            TestOutcome::Unable => keys::TEST_UNABLE,
        }
    }
}

/// Runs `ProviderKeys::test_llm` (a model-list probe with the saved key and a
/// 10 s timeout; no prompt and no user text is sent). Blocking.
pub fn test_connection<C: lf_providers::LlmConfig>(
    secrets: &dyn SecretStore,
    configs: &dyn ConfigStore<C>,
    provider_id: &str,
    base_url: Option<&str>,
    model: &str,
) -> TestOutcome {
    let (ok, message) = ProviderKeys::new(secrets, configs).test_llm(provider_id, base_url, model);
    match (ok, message.as_str()) {
        (true, _) => TestOutcome::Ok,
        (false, m) if m == keys::TEST_FAILED => TestOutcome::Failed,
        _ => TestOutcome::Unable,
    }
}

/// Why Test connection is disabled for a provider row.
pub fn test_blocker(
    needs_key: bool,
    has_key: bool,
    env_override: bool,
    model: &str,
) -> Option<&'static str> {
    if needs_key && !has_key && !env_override {
        Some("Save a key first")
    } else if model.trim().is_empty() && needs_key {
        Some("Choose a model first")
    } else {
        None
    }
}

/// The sandbox's fake environment: example variable names only.
pub fn sandbox_environment() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert(
        "OPENAI_API_KEY".into(),
        "sandbox-example-openai-0000".into(),
    );
    env.insert("GROQ_API_KEY".into(), "sandbox-example-groq-0000".into());
    env.insert("PATH".into(), "/usr/bin".into());
    env
}

/// Accept a typed path (portal-less fallback): `~` expands, must be a file.
pub fn typed_path(text: &str) -> Result<PathBuf, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Type the path of a .env file.".into());
    }
    let path = lf_core::home::expand_user(Path::new(text)).map_err(|e| e.to_string())?;
    if !path.is_file() {
        return Err(format!("{} isn't a file.", path.display()));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{ConfigFile, DataRoot, MemorySecrets};

    const SECRET_A: &str = "sk-unit-test-value-AAAA1111";
    const SECRET_B: &str = "gsk-unit-test-value-BBBB2222";

    fn env() -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert("OPENAI_API_KEY".into(), SECRET_A.into());
        env.insert("GROQ_API_KEY".into(), SECRET_B.into());
        env.insert("HOME".into(), "/home/x".into());
        env
    }

    fn no_override(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn environment_import_counts_defaults_and_never_exposes_values() {
        let dir = tempfile::tempdir().unwrap();
        let file = ConfigFile::new(DataRoot::sandbox(dir.path()));
        let secrets = MemorySecrets::default();
        secrets.set("provider_key_groq", "existing").unwrap();
        let import = KeyImport::detect(Source::Environment, &env()).unwrap();
        assert_eq!(import.summary(), "Found 2 keys for 2 providers.");
        let has_key = |id: &str| !secrets.get(&format!("provider_key_{id}")).is_empty();
        let rows = import.rows(&has_key, &no_override);
        let openai = rows.iter().find(|r| r.provider_id == "openai").unwrap();
        let groq = rows.iter().find(|r| r.provider_id == "groq").unwrap();
        assert!(openai.selected && !openai.replaces);
        assert!(!groq.selected && groq.replaces, "saved key: off by default");
        let shown = format!("{rows:?} {import:?} {}", import.summary());
        assert!(!shown.contains(SECRET_A) && !shown.contains(SECRET_B));
        assert!(
            !shown.contains("AAAA") && !shown.contains("BBBB"),
            "not even masked"
        );

        let config_before = file.load().unwrap();
        let keys = ProviderKeys::new(&secrets, &file);
        let imported = import.import(&keys, &has_key, &no_override).unwrap();
        assert_eq!(imported, vec!["OpenAI".to_string()]);
        assert_eq!(secrets.get("provider_key_openai"), SECRET_A);
        assert_eq!(
            secrets.get("provider_key_groq"),
            "existing",
            "unticked: untouched"
        );
        let config_after = file.load().unwrap();
        assert_eq!(
            config_after.field_str("llm_provider"),
            config_before.field_str("llm_provider")
        );
        assert_eq!(
            config_after.field_str("llm_model"),
            config_before.field_str("llm_model")
        );
    }

    #[test]
    fn dotenv_file_import_overwrite_intent_and_one_source_per_provider() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(
            &path,
            format!("export OPENAI_API_KEY={SECRET_A}\nOPENAI_KEY=$OTHER\n"),
        )
        .unwrap();
        let mut import = KeyImport::detect(Source::File(path.clone()), &env()).unwrap();
        assert_eq!(
            import.summary(),
            "Found 1 key for 1 provider.",
            "environment ignored for a file source"
        );
        let rows = import.rows(&|_| true, &no_override);
        assert!(!rows[0].selected && rows[0].replaces);
        assert!(rows[0].source.ends_with("OPENAI_API_KEY"));
        import.toggle(&rows[0].key, true);
        assert_eq!(import.selected_count(&|_| true, &no_override), 1);

        let err =
            KeyImport::detect(Source::File(dir.path().join("missing.env")), &env()).unwrap_err();
        assert!(err.contains("doesn't exist") && !err.contains(SECRET_A));
        let empty = KeyImport::detect(Source::Environment, &BTreeMap::new()).unwrap();
        assert!(empty.is_empty());
        assert!(empty.summary().contains("No provider keys found"));
        assert!(empty
            .import(
                &ProviderKeys::new(
                    &MemorySecrets::default(),
                    &ConfigFile::new(DataRoot::sandbox(dir.path()))
                ),
                &|_| false,
                &no_override
            )
            .is_err());
    }

    #[test]
    fn environment_overrides_block_ineffective_imports() {
        let import = KeyImport::detect(Source::Environment, &env()).unwrap();
        let override_openai =
            |id: &str| (id == "openai").then(|| "LOCALFLOW_PROVIDER_KEY_OPENAI".to_string());
        let rows = import.rows(&|_| false, &override_openai);
        let openai = rows.iter().find(|r| r.provider_id == "openai").unwrap();
        assert!(openai
            .blocked
            .as_deref()
            .unwrap()
            .contains("LOCALFLOW_PROVIDER_KEY_OPENAI"));
        assert!(!openai.selected);
        assert_eq!(import.selected_count(&|_| false, &override_openai), 1);
    }

    #[test]
    fn test_connection_only_reports_fixed_text_and_is_blocked_without_key() {
        let dir = tempfile::tempdir().unwrap();
        let file = ConfigFile::new(DataRoot::sandbox(dir.path()));
        let secrets = MemorySecrets::default();
        // No key saved: refused before any network use.
        assert_eq!(
            test_connection(&secrets, &file, "openai", None, "gpt-5-mini"),
            TestOutcome::Unable
        );
        assert_eq!(
            test_blocker(true, false, false, "m"),
            Some("Save a key first")
        );
        assert_eq!(
            test_blocker(true, true, false, " "),
            Some("Choose a model first")
        );
        assert_eq!(test_blocker(false, false, false, ""), None);
        for outcome in [TestOutcome::Ok, TestOutcome::Failed, TestOutcome::Unable] {
            assert!([keys::TEST_OK, keys::TEST_FAILED, keys::TEST_UNABLE].contains(&outcome.text()));
        }
        // A local endpoint that refuses connections: a fixed failure line.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let outcome = test_connection(
            &secrets,
            &file,
            "custom",
            Some(&format!("http://127.0.0.1:{port}/v1")),
            "m",
        );
        assert_ne!(outcome, TestOutcome::Ok);
    }
}
