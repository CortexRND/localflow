//! Models & keys: the asynchronous provider operations' bookkeeping
//! (GPUI-free so stale/overlapping completions are unit-tested).
//!
//! - [`ImportSession`]: the "Import keys" card. Every open, close, source
//!   switch, picker launch, tab or page change bumps a generation; a scan or
//!   import that finishes for an older generation is dropped, so old key
//!   candidates never land in a newer card.
//! - [`OpGate`]: one provider mutation (save key, use, remove, import) at a
//!   time; completions carry the op's generation.
//! - [`TestTarget`]: what Test connection really probes, shared by the
//!   Cleanup tab and the provider rows (and their tooltips).
//! - Sandbox fixtures: the Ollama list read through the shared simulated
//!   backend, and a deterministic `.env` fixture inside the sandbox.

use std::path::{Path, PathBuf};

use lf_core::config::Config;
use lf_providers::keys::Preset;
use lf_providers::{ConfigStore, SecretStore};
use lf_ui::models_layout::{self, ListState};
use lf_ui::setup_backend::SetupModelBackend;

use crate::controllers::key_import::{KeyImport, TestOutcome};

// ---------------------------------------------------------- import card ---

#[derive(Debug, Default)]
pub struct ImportCard {
    pub file: bool,
    pub data: Option<KeyImport>,
    pub error: Option<String>,
    pub saving: bool,
}

/// What an import completion did.
#[derive(Debug, PartialEq, Eq)]
pub enum ImportDone {
    /// Saved: the card closed. Labels of the imported providers.
    Imported(Vec<String>),
    /// Failed: the card keeps its candidates and shows the error.
    Failed,
    /// For a card that no longer exists: nothing changed, candidates dropped.
    Stale,
}

#[derive(Debug, Default)]
pub struct ImportSession {
    generation: u64,
    pub card: Option<ImportCard>,
}

impl ImportSession {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Open (or reopen) the card; scan results of older cards are stale.
    pub fn open(&mut self) -> u64 {
        self.generation += 1;
        self.card = Some(ImportCard::default());
        self.generation
    }

    /// Close (Cancel, tab or page change): candidates are dropped.
    pub fn close(&mut self) {
        self.generation += 1;
        self.card = None;
    }

    pub fn saving(&self) -> bool {
        self.card.as_ref().is_some_and(|c| c.saving)
    }

    /// A new scan (source switch, picker launch, typed path): clears the
    /// old candidates. `None` while an import is being saved (the source
    /// can't change under a commit) or with no card.
    pub fn begin_scan(&mut self, file: bool) -> Option<u64> {
        let card = self.card.as_mut()?;
        if card.saving {
            return None;
        }
        self.generation += 1;
        card.file = file;
        card.data = None;
        card.error = None;
        Some(self.generation)
    }

    /// A scan finished; applied only to the card generation it was for.
    pub fn scan_done(&mut self, generation: u64, result: Result<KeyImport, String>) -> bool {
        if generation != self.generation {
            return false;
        }
        let Some(card) = self.card.as_mut() else {
            return false;
        };
        match result {
            Ok(data) => {
                card.data = Some(data);
                card.error = None;
            }
            Err(error) => {
                card.data = None;
                card.error = Some(error);
            }
        }
        true
    }

    /// Import pressed: the candidates move to the worker.
    pub fn begin_import(&mut self) -> Option<(u64, KeyImport)> {
        let card = self.card.as_mut()?;
        if card.saving {
            return None;
        }
        let data = card.data.take()?;
        card.saving = true;
        Some((self.generation, data))
    }

    /// The import worker returned. A stale completion changes nothing and
    /// its candidates are dropped (never restored into a newer card).
    pub fn import_done(
        &mut self,
        generation: u64,
        data: KeyImport,
        result: Result<Vec<String>, String>,
    ) -> ImportDone {
        if generation != self.generation || self.card.is_none() {
            drop(data);
            return ImportDone::Stale;
        }
        match result {
            Ok(labels) => {
                self.close();
                ImportDone::Imported(labels)
            }
            Err(error) => {
                if let Some(card) = self.card.as_mut() {
                    card.data = Some(data);
                    card.error = Some(error);
                    card.saving = false;
                }
                ImportDone::Failed
            }
        }
    }
}

// ------------------------------------------------------- mutation gate ---

/// One provider mutation at a time (keyring writes can't complete out of
/// order). Each op gets a generation; its completion is matched against it.
#[derive(Debug, Default)]
pub struct OpGate {
    generation: u64,
    running: Option<u64>,
}

impl OpGate {
    pub fn busy(&self) -> bool {
        self.running.is_some()
    }

    pub fn begin(&mut self) -> Option<u64> {
        if self.running.is_some() {
            return None;
        }
        self.generation += 1;
        self.running = Some(self.generation);
        Some(self.generation)
    }

    /// The op finished; `true` when it is the one that was running.
    pub fn finish(&mut self, generation: u64) -> bool {
        if self.running == Some(generation) {
            self.running = None;
            true
        } else {
            false
        }
    }
}

// ------------------------------------------------------- editor lifetime ---

/// The one open provider editor (key or Use form). Every open is a new
/// instance with its own generation, so a mutation finishing later can only
/// close the very editor it was submitted from — never one reopened for the
/// same provider meanwhile. Holds no draft values (key text never lives
/// here).
#[derive(Debug)]
pub struct EditorSlot<K> {
    open: Option<(K, u64)>,
    generation: u64,
}

impl<K> Default for EditorSlot<K> {
    fn default() -> Self {
        EditorSlot {
            open: None,
            generation: 0,
        }
    }
}

impl<K: Copy + PartialEq> EditorSlot<K> {
    /// Open a fresh editor instance (replacing any open one).
    pub fn open(&mut self, kind: K) -> (K, u64) {
        self.generation += 1;
        self.open = Some((kind, self.generation));
        (kind, self.generation)
    }

    pub fn close(&mut self) {
        self.open = None;
    }

    pub fn current(&self) -> Option<K> {
        self.open.map(|(kind, _)| kind)
    }

    /// The open instance, to bind a mutation to.
    pub fn ticket(&self) -> Option<(K, u64)> {
        self.open
    }

    /// A mutation submitted from `ticket` succeeded. Close the editor only
    /// when it is still that same instance and its draft wasn't touched
    /// since submit; otherwise the newer editor and its draft stay.
    pub fn close_after_success(&mut self, ticket: (K, u64), draft_untouched: bool) -> bool {
        if self.open == Some(ticket) && draft_untouched {
            self.open = None;
            true
        } else {
            false
        }
    }
}

// --------------------------------------------------------- test target ---

/// What Test connection probes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestTarget {
    /// The provider cleanup really uses now: the saved config's provider,
    /// endpoint and model (built like the runtime builds it).
    Configured {
        label: String,
        endpoint: String,
        model: String,
    },
    /// An inactive preset row: its default endpoint and model.
    Preset {
        id: &'static str,
        endpoint: &'static str,
        model: String,
        needs_key: bool,
    },
}

impl TestTarget {
    /// Key for per-target results (the active configuration has its own).
    pub fn key(&self) -> &'static str {
        match self {
            TestTarget::Configured { .. } => "configured",
            TestTarget::Preset { id, .. } => id,
        }
    }

    pub fn endpoint(&self) -> &str {
        match self {
            TestTarget::Configured { endpoint, .. } => endpoint,
            TestTarget::Preset { endpoint, .. } => endpoint,
        }
    }

    pub fn tooltip(&self) -> String {
        let label = match self {
            TestTarget::Configured { label, .. } => label.clone(),
            TestTarget::Preset { id, .. } => lf_providers::keys::get_preset(id)
                .map(|p| p.label.to_string())
                .unwrap_or_else(|_| id.to_string()),
        };
        format!(
            "Contacts {label} at {}. Lists models only; no text is sent.",
            self.endpoint()
        )
    }
}

/// The target for the current cleanup configuration (Cleanup tab, and the
/// active provider's row), whatever host it points at.
pub fn configured_target(config: &Config) -> TestTarget {
    let provider = config.field_str("llm_provider").unwrap_or("");
    let label = models_layout::active_preset(config)
        .map(|p| p.label.to_string())
        .unwrap_or_else(|| match provider {
            "ollama" => "Ollama".to_string(),
            other => other.to_string(),
        });
    TestTarget::Configured {
        label,
        endpoint: config.field_str("llm_base_url").unwrap_or("").to_string(),
        model: config.field_str("llm_model").unwrap_or("").to_string(),
    }
}

/// The target for a provider row: the real configuration when the row is
/// active, else the preset's defaults.
pub fn row_target(preset: &'static Preset, config: &Config) -> TestTarget {
    if models_layout::is_active(preset, config, None) {
        return configured_target(config);
    }
    TestTarget::Preset {
        id: preset.id,
        endpoint: preset.base_url,
        model: preset.default_model.to_string(),
        needs_key: preset.needs_key,
    }
}

/// Seconds a configured-provider probe may take: the same bound as
/// `ProviderKeys::test_llm`, never the cleanup (inference) timeout.
pub const PROBE_TIMEOUT_SECS: i64 = 10;

/// Run the probe (blocking; a worker). The configured target is built with
/// the runtime's own factory (`lf_providers::factory::build_llm`, so the
/// configured endpoint and its endpoint-bound key are used) with a
/// [`PROBE_TIMEOUT_SECS`] timeout and asked for its model list
/// (`healthcheck`); a preset row uses `ProviderKeys::test_llm`. Only the
/// three fixed outcomes come back.
pub fn run_test<C: lf_providers::LlmConfig>(
    target: &TestTarget,
    secrets: &dyn SecretStore,
    configs: &dyn ConfigStore<C>,
    config: &C,
) -> TestOutcome {
    match target {
        TestTarget::Configured { .. } => {
            match lf_providers::factory::build_llm(
                config,
                &lf_providers::factory::BuildOptions::with_timeout(PROBE_TIMEOUT_SECS),
                secrets,
            ) {
                Ok(provider) if provider.healthcheck().ok => TestOutcome::Ok,
                Ok(_) => TestOutcome::Failed,
                Err(_) => TestOutcome::Unable,
            }
        }
        TestTarget::Preset { id, model, .. } => {
            crate::controllers::key_import::test_connection(secrets, configs, id, None, model)
        }
    }
}

// ------------------------------------------------------------- sandbox ---

/// The sandbox Ollama list, read through the shared simulated backend (so
/// a cleanup pull made in setup shows up here). Blocking; a worker.
pub fn ollama_listing(backend: &dyn SetupModelBackend, base_url: &str) -> (Vec<String>, ListState) {
    match backend.ollama(base_url) {
        Ok((_, models)) if models.is_empty() => (models, ListState::Empty),
        Ok((_, models)) => (models, ListState::Ready),
        Err(_) => (Vec::new(), ListState::Unavailable),
    }
}

/// The sandbox's deterministic `.env` example (example values only).
pub const SANDBOX_ENV_FIXTURE: &str = "# LocalFlow sandbox example — not real keys\n\
OPENAI_API_KEY=sandbox-example-openai-file-0000\n\
GROQ_API_KEY=sandbox-example-groq-file-0000\n";

/// Write (if missing) and return the sandbox's example `.env` file, inside
/// the sandbox directory. The sandbox scans only this, whatever file the
/// user picked or typed.
pub fn sandbox_env_file(sandbox_dir: &Path) -> std::io::Result<PathBuf> {
    let path = sandbox_dir.join("examples").join("provider-keys.env");
    if !path.is_file() {
        std::fs::create_dir_all(path.parent().unwrap_or(sandbox_dir))?;
        std::fs::write(&path, SANDBOX_ENV_FIXTURE)?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Ed {
        Key(&'static str),
        Use(&'static str),
    }

    #[test]
    fn a_late_success_never_closes_a_reopened_or_edited_editor() {
        // Save key A → Cancel → reopen key A (new draft) → old save ends.
        let mut slot = EditorSlot::default();
        let submitted = slot.open(Ed::Key("openai"));
        slot.close();
        slot.open(Ed::Key("openai"));
        assert!(!slot.close_after_success(submitted, true));
        assert_eq!(slot.current(), Some(Ed::Key("openai")), "newer editor kept");

        // Same for Use provider A, without even cancelling: reopen in place.
        let submitted = slot.open(Ed::Use("openai"));
        slot.open(Ed::Use("openai"));
        assert!(!slot.close_after_success(submitted, true));
        assert_eq!(slot.current(), Some(Ed::Use("openai")));

        // Same instance, but the user typed a new draft while it saved.
        let submitted = slot.ticket().unwrap();
        assert!(!slot.close_after_success(submitted, false));
        assert_eq!(slot.current(), Some(Ed::Use("openai")), "edited draft kept");

        // Same instance, untouched: the editor closes.
        assert!(slot.close_after_success(submitted, true));
        assert_eq!(slot.current(), None);

        // A different provider's editor is never closed either.
        let submitted = slot.open(Ed::Key("groq"));
        slot.open(Ed::Key("openai"));
        assert!(!slot.close_after_success(submitted, true));
        assert_eq!(slot.current(), Some(Ed::Key("openai")));
    }
    use crate::controllers::key_import::Source;
    use std::collections::BTreeMap;

    fn scan(env: &[(&str, &str)]) -> KeyImport {
        let map: BTreeMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        KeyImport::detect(Source::Environment, &map).unwrap()
    }

    #[test]
    fn a_delayed_old_scan_never_fills_a_newer_card() {
        let mut session = ImportSession::default();
        let first = session.open();
        // Closed and reopened while the first scan was still running.
        session.close();
        let second = session.open();
        assert!(!session.scan_done(first, Ok(scan(&[("OPENAI_API_KEY", "sk-old-value-111")]))));
        assert!(session.card.as_ref().unwrap().data.is_none());
        // A source switch also supersedes the pending scan.
        let file_scan = session.begin_scan(true).unwrap();
        assert!(!session.scan_done(second, Ok(scan(&[("GROQ_API_KEY", "gsk-old-222")]))));
        assert!(session.scan_done(file_scan, Ok(scan(&[("GROQ_API_KEY", "gsk-new-333")]))));
        assert!(session.card.as_ref().unwrap().data.is_some());
    }

    #[test]
    fn import_completions_only_touch_their_own_card() {
        let mut session = ImportSession::default();
        let g = session.open();
        assert!(session.scan_done(g, Ok(scan(&[("OPENAI_API_KEY", "sk-aaaa-1111")]))));
        let (generation, data) = session.begin_import().unwrap();
        assert!(session.begin_import().is_none(), "one commit at a time");
        assert_eq!(
            session.begin_scan(false),
            None,
            "source is locked while saving"
        );
        // The user navigates away (card closed) and opens a new one.
        session.close();
        session.open();
        assert_eq!(
            session.import_done(generation, data, Err("keyring locked".into())),
            ImportDone::Stale
        );
        let card = session.card.as_ref().unwrap();
        assert!(
            card.data.is_none() && card.error.is_none(),
            "old candidates not restored"
        );
        // A matching completion: failure restores, success closes.
        let g = session.begin_scan(false).unwrap();
        session.scan_done(g, Ok(scan(&[("OPENAI_API_KEY", "sk-bbbb-2222")])));
        let (generation, data) = session.begin_import().unwrap();
        assert_eq!(
            session.import_done(generation, data, Err("keyring locked".into())),
            ImportDone::Failed
        );
        assert!(session.card.as_ref().unwrap().data.is_some());
        let (generation, data) = session.begin_import().unwrap();
        assert_eq!(
            session.import_done(generation, data, Ok(vec!["OpenAI".into()])),
            ImportDone::Imported(vec!["OpenAI".into()])
        );
        assert!(session.card.is_none());
        assert!(
            !format!("{session:?}").contains("sk-"),
            "no values in Debug"
        );
    }

    #[test]
    fn provider_mutations_are_serialized() {
        let mut gate = OpGate::default();
        let save = gate.begin().unwrap();
        assert!(gate.busy());
        assert_eq!(gate.begin(), None, "a second write waits");
        assert!(
            !gate.finish(save + 1),
            "a foreign completion doesn't release"
        );
        assert!(gate.finish(save));
        let next = gate.begin().unwrap();
        assert!(next > save);
    }

    #[test]
    fn test_targets_follow_the_active_configuration() {
        let ollama = lf_providers::keys::get_preset("ollama").unwrap();
        let openai = lf_providers::keys::get_preset("openai").unwrap();
        // Ollama on another host: the Cleanup tab probes that host.
        let mut config = Config::defaults_for(false);
        config.set_field("llm_provider", serde_json::json!("ollama"));
        config.set_field("llm_base_url", serde_json::json!("http://gpu-box:11434"));
        config.set_field("llm_model", serde_json::json!("qwen3:4b"));
        let target = configured_target(&config);
        assert_eq!(target.endpoint(), "http://gpu-box:11434");
        assert!(target.tooltip().contains("http://gpu-box:11434"));
        assert!(matches!(&target, TestTarget::Configured { model, .. } if model == "qwen3:4b"));
        // That Ollama row isn't the preset's default host: it tests defaults.
        assert_eq!(row_target(ollama, &config).endpoint(), ollama.base_url);
        // An active named provider: its row and the tab probe the config.
        config.set_field("llm_provider", serde_json::json!(openai.llm_provider));
        config.set_field("llm_base_url", serde_json::json!(openai.base_url));
        config.set_field("llm_model", serde_json::json!("gpt-5-mini"));
        assert!(
            matches!(row_target(openai, &config), TestTarget::Configured { model, .. } if model == "gpt-5-mini")
        );
        // An inactive row uses the preset's defaults.
        assert!(matches!(
            row_target(ollama, &config),
            TestTarget::Preset { id: "ollama", .. }
        ));
    }

    #[test]
    fn the_sandbox_ollama_list_follows_pulls_through_the_shared_backend() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let backend = crate::sandbox::SimSetupBackend::with_state(
            crate::sandbox::SimSpeech::seeded(),
            std::time::Duration::from_millis(1),
        );
        let (before, state) = ollama_listing(&backend, "");
        assert_eq!(state, ListState::Ready);
        assert!(!before.contains(&"gemma3:4b".to_string()));
        assert!(backend
            .pull_cleanup(
                "",
                "gemma3:4b",
                Box::new(|_| {}),
                Arc::new(AtomicBool::new(true))
            )
            .is_err());
        assert_eq!(
            ollama_listing(&backend, "").0,
            before,
            "a cancelled pull adds nothing"
        );
        backend
            .pull_cleanup(
                "",
                "gemma3:4b",
                Box::new(|_| {}),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert!(ollama_listing(&backend, "")
            .0
            .contains(&"gemma3:4b".to_string()));
    }

    #[test]
    fn the_sandbox_env_fixture_lives_in_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let path = sandbox_env_file(dir.path()).unwrap();
        assert!(path.starts_with(dir.path()));
        let found = KeyImport::detect(Source::File(path.clone()), &BTreeMap::new()).unwrap();
        assert_eq!(found.summary(), "Found 2 keys for 2 providers.");
        assert_eq!(sandbox_env_file(dir.path()).unwrap(), path, "idempotent");
    }
}
