//! `--sandbox`: a persistent, isolated data directory with example data.
//!
//! The directory is seeded once (marker file) and kept between launches so
//! edits can be checked after a relaunch. `--sandbox-reset` re-seeds it.
//! Services are in-process fakes; no socket is opened and the OS keyring is
//! never used.

use std::fs;
use std::path::Path;

use anyhow::{Context as _, Result};
use lf_core::config::{self, Config};
use lf_core::dictionary::DictionaryStore;
use lf_core::history::NewEntry;
use lf_core::hotphrases::HotPhraseStore;
use lf_ui::backends::meeting::FakeMeetingClient;
use lf_ui::backends::prompts::{FakePromptQueue, PromptQueue, WorkPrompt};
use serde_json::{json, Map, Value};

use crate::data::{open_history, DataRoot, Services, SANDBOX_MARKER};

const MARKER_TEXT: &str = "This directory is a LocalFlow desktop sandbox.\n\
It holds example data only. Nothing here is your real LocalFlow data.\n\
Delete it at any time, or run `lf-desktop --sandbox --sandbox-reset`.\n";

/// Example dictations: `(minutes ago, app, category, raw, final, status)`.
const HISTORY: &[(i64, &str, &str, &str, &str, &str)] = &[
    (
        12,
        "Mail",
        "email",
        "hi sam thanks for the notes from yesterday i will send the revised budget by friday",
        "Hi Sam, thanks for the notes from yesterday. I'll send the revised budget by Friday.",
        "ok",
    ),
    (
        47,
        "Terminal",
        "work",
        "git commit dash m fix the off by one in the pager",
        "git commit -m \"Fix the off-by-one in the pager\"",
        "ok",
    ),
    (
        95,
        "Editor",
        "work",
        "todo move the sandbox seed into its own module and add a reset flag",
        "TODO: move the sandbox seed into its own module and add a reset flag.",
        "ok",
    ),
    (
        180,
        "Chat",
        "personal",
        "running ten minutes late see you at the cafe",
        "Running ten minutes late, see you at the café!",
        "ok",
    ),
    (240, "Editor", "work", "", "", "silent"),
    // Answers the built-in "What have I dictated about the launch?".
    (
        300,
        "Notes",
        "work",
        "launch plan announce the desktop preview on monday and share the release notes with the team",
        "Launch plan: announce the desktop preview on Monday and share the release notes with the team.",
        "ok",
    ),
    (
        1500,
        "Browser",
        "other",
        "search for local speech recognition benchmarks twenty twenty six",
        "Search for local speech recognition benchmarks 2026",
        "ok",
    ),
    (
        1560,
        "Notes",
        "personal",
        "ideas for the trip hiking in the dolomites book the hut early",
        "Ideas for the trip: hiking in the Dolomites, book the hut early.",
        "ok",
    ),
    (
        1620,
        "Mail",
        "email",
        "the quarterly review moved to thursday at two",
        "The quarterly review moved to Thursday at 2 PM.",
        "ok",
    ),
    (
        2950,
        "Editor",
        "work",
        "kubernetes rollout failed on the canary check the readiness probe",
        "Kubernetes rollout failed on the canary; check the readiness probe.",
        "ok",
    ),
    (
        3000,
        "Notes",
        "other",
        "会議は木曜日の午後二時に移動しました",
        "会議は木曜日の午後二時に移動しました。",
        "ok",
    ),
    (
        3010,
        "Notes",
        "other",
        "تم نقل الاجتماع إلى يوم الخميس",
        "تم نقل الاجتماع إلى يوم الخميس.",
        "ok",
    ),
    (4400, "Editor", "work", "", "", "failed"),
    (
        5800,
        "Chat",
        "personal",
        "happy birthday hope you have a great day",
        "Happy birthday! Hope you have a great day.",
        "ok",
    ),
    (
        7300,
        "Mail",
        "email",
        "please find the signed contract attached",
        "Please find the signed contract attached.",
        "ok",
    ),
];

const WORDS: &[&str] = &["LocalFlow", "Kubernetes", "Ollama", "Dolomites", "GPUI"];
const REPLACEMENTS: &[(&str, &str)] = &[
    ("teh", "the"),
    ("my email", "sam@example.com"),
    ("k8s", "Kubernetes"),
];
const SNIPPETS: &[(&str, &str, bool)] = &[
    ("my address", "221B Example Street, Springfield", true),
    (
        "standup template",
        "Yesterday:\n- \nToday:\n- \nBlockers:\n- ",
        true,
    ),
    ("sign off", "Best regards,\nSam", false),
];

/// Entries of `dir` other than the live instance directory (which holds the
/// window lock and must survive a reset).
fn content_entries(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        if entry.file_name() != crate::controllers::instance::SANDBOX_DIR {
            out.push(entry.path());
        }
    }
    Ok(out)
}

/// `dir` may be used as a sandbox: absent, empty, or seeded by us. Call
/// before taking the instance lock inside it.
pub fn check_dir(dir: &Path) -> Result<()> {
    if dir.join(SANDBOX_MARKER).exists() || content_entries(dir)?.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "{} exists and is not a LocalFlow sandbox; choose an empty directory",
        dir.display()
    )
}

/// Seed `dir` unless already seeded (or `reset`). Returns the data root.
///
/// The caller holds the sandbox's instance lock, so no other window uses
/// the data being reset; the instance directory itself is kept.
pub fn prepare(dir: &Path, reset: bool) -> Result<DataRoot> {
    let marker = dir.join(SANDBOX_MARKER);
    if reset && dir.exists() {
        // Only ever delete a directory we seeded ourselves.
        anyhow::ensure!(
            marker.exists() || content_entries(dir)?.is_empty(),
            "{} is not a LocalFlow sandbox (no {SANDBOX_MARKER}); refusing to reset it",
            dir.display()
        );
        for path in content_entries(dir)? {
            let removed = if path.is_dir() && !path.is_symlink() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            removed.with_context(|| format!("remove {}", path.display()))?;
        }
    }
    let root = DataRoot::sandbox(dir);
    if marker.exists() {
        return Ok(root);
    }
    check_dir(dir)?;
    fs::create_dir_all(&root.localflow_dir)?;
    seed(&root)?;
    fs::write(&marker, MARKER_TEXT)?;
    Ok(root)
}

fn seed(root: &DataRoot) -> Result<()> {
    let base = root.sandbox_dir.clone().expect("sandbox root");
    let vault = base.join("vault");
    let notes = vault.join("notes").join("meetings");
    fs::create_dir_all(&notes)?;
    fs::write(
        notes.join("2026-11-18 1000 Roadmap sync.md"),
        "# Roadmap sync (example)\n\n## Summary\n- Native desktop app ships beside the existing Hub.\n\n## Action items\n- [ ] Review the cutover checklist\n",
    )?;
    fs::write(
        notes.join("2026-11-12 1530 Design review.md"),
        "# Design review (example)\n\n## Summary\n- Warm stone palette, forest accent.\n",
    )?;
    fs::create_dir_all(base.join("templates"))?;

    let mut cfg = Config::defaults_for(cfg!(target_os = "macos"));
    let set = |cfg: &mut Config, field: &str, value: Value| cfg.set_field(field, value);
    set(&mut cfg, "meetings_enabled", json!(true));
    set(&mut cfg, "skills_dirs", json!([]));
    set(&mut cfg, "vault_path", json!(vault.to_string_lossy()));
    set(
        &mut cfg,
        "prompts_dir",
        json!(base.join("prompts").to_string_lossy()),
    );
    set(
        &mut cfg,
        "meeting_templates_dir",
        json!(base.join("templates").to_string_lossy()),
    );
    // Port 9 (discard) is never served: the sandbox makes no HTTP calls anyway.
    set(&mut cfg, "server_port", json!(9));
    config::save_config(&cfg, Some(&root.config_path))?;

    let history = open_history(root)?;
    let now = chrono::Utc::now().timestamp() as f64;
    for (minutes, app, category, raw, text, status) in HISTORY {
        let mut entry = NewEntry::new("dictate");
        entry.app_name = Some((*app).into());
        entry.category = Some((*category).into());
        entry.raw_text = (*raw).into();
        entry.final_text = (*text).into();
        entry.status = (*status).into();
        entry.duration_s = 4.0 + (text.len() as f64 / 12.0);
        entry.ts = Some(now - (*minutes as f64) * 60.0);
        if *status == "failed" {
            // A synthetic tone (well above the -50 dBFS silence gate) so the
            // failed example can be retried; written inside the sandbox clips.
            let tone: Vec<f32> = (0..24_000)
                .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 220.0 / 16_000.0).sin() * 0.1)
                .collect();
            entry.duration_s = tone.len() as f64 / 16_000.0;
            // Persist an absolute path: a relative `--sandbox` root would
            // otherwise store a cwd-relative path that breaks on relaunch.
            let clip = history.write_clip(&tone, 16_000)?;
            let clip = persisted_clip_path(&clip)
                .with_context(|| format!("resolve {}", clip.display()))?;
            entry.audio_path = Some(clip.to_string_lossy().into());
        }
        history.add(&entry)?;
    }

    let dictionary = DictionaryStore::new(Some(root.dictionary_path.clone()));
    for word in WORDS {
        dictionary.add_word(word)?;
    }
    for (from, to) in REPLACEMENTS {
        dictionary.add_replacement(from, to)?;
    }
    let phrases = HotPhraseStore::new(root.hot_phrases_path.clone());
    for (trigger, text, enabled) in SNIPPETS {
        phrases.add(trigger, text, *enabled)?;
    }
    Ok(())
}

/// The absolute form of a freshly written clip path, stable across a cwd
/// change and in the same prefix style as the (possibly relative) clips root,
/// so `HistoryStore`'s containment check still matches it.
///
/// Windows: `std::path::absolute` (GetFullPathNameW) only prepends the cwd and
/// normalizes lexically. `fs::canonicalize` would return a `\\?\C:\...`
/// verbatim path that never `starts_with` an ordinary `C:\...` clips root.
#[cfg(windows)]
fn persisted_clip_path(clip: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    std::path::absolute(clip)
}

/// Elsewhere: `fs::canonicalize`, so a relative root reached through `..`
/// doesn't store the old working directory (which may be deleted) as a prefix.
#[cfg(not(windows))]
fn persisted_clip_path(clip: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    fs::canonicalize(clip)
}

/// The sandbox prompt queue: three example prompts, one failed.
pub fn seed_prompt_queue() -> FakePromptQueue {
    let queue = FakePromptQueue::new();
    let prompts = [
        WorkPrompt::new("Add a reset flag to the sandbox", "Example prompt."),
        WorkPrompt::new("Write the cutover checklist", "Example prompt."),
        WorkPrompt::new("Update deploy docs for Redis", "Example prompt."),
    ];
    let paths: Vec<String> = (1..=3).map(|i| format!("example/prompt-{i}.md")).collect();
    if let Ok(entries) = queue.add(&prompts, &paths, "Roadmap sync (example)") {
        if let Some(id) = entries
            .get(2)
            .and_then(|e| e.get("id"))
            .and_then(Value::as_str)
        {
            let mut fields = Map::new();
            fields.insert("error".into(), json!("Orca is not running"));
            let _ = queue.set_status(id, "failed", None, &fields);
        }
    }
    queue
}

/// The sandbox meeting client: saved notes point into the sandbox vault.
pub fn meeting_client(root: &DataRoot) -> FakeMeetingClient {
    let client = FakeMeetingClient::new();
    if let Some(base) = &root.sandbox_dir {
        let notes = base
            .join("vault")
            .join("notes")
            .join("meetings")
            .join("2026-11-18 1000 Roadmap sync.md");
        client.set_saved_notes_path(&notes.to_string_lossy());
    }
    client.set_chat_answer(Some(
        "Sandbox answer: the example meeting agreed to ship the native app beside the existing Hub.",
    ));
    client
}

// ---------------------------------------------------- --sandbox-exercise ---

/// The trigger `--sandbox-exercise` adds to the sandbox Hot Phrases.
pub const EXERCISE_PHRASE: &str = "cutover gate phrase";

/// `--sandbox-exercise`: one write per sandbox store through the same
/// controllers the pages call, each read back from the sandbox. Returns one
/// line per store; `scripts/gpui_data_safety.py` checks the real stores.
pub fn exercise(services: &Services) -> Result<Vec<String>> {
    use crate::controllers::{history, models, prompts, settings};
    use lf_ui::hot_phrases_editor::HotPhrasesEditor;

    anyhow::ensure!(
        services.is_sandbox(),
        "the exercise only runs in the sandbox"
    );
    let mut done = Vec::new();

    let spec = settings::spec("dictation", "enabled").context("no Dictation setting")?;
    let enabled = !services
        .config()
        .field_bool("dictation_enabled")
        .unwrap_or(true);
    services.set_cached_config(settings::save(&services.config_file, spec, json!(enabled))?);
    anyhow::ensure!(
        services.config_file.load()?.field_bool("dictation_enabled") == Some(enabled),
        "the Settings change did not reach the sandbox config"
    );
    done.push(format!("settings: dictation enabled = {enabled}"));

    let key = "sandbox-exercise-key";
    models::save_key_and_apply(
        services.secrets.as_ref(),
        &services.config_file,
        "groq",
        key,
    )?;
    anyhow::ensure!(
        services.secrets.get("provider_key_groq") == key,
        "the provider key did not reach the sandbox secrets"
    );
    done.push("models: groq key saved to the in-memory sandbox keyring".into());

    let mut editor = HotPhrasesEditor::new(services.phrases_model()?);
    anyhow::ensure!(editor.new_phrase(), "couldn't start a new hot phrase");
    editor.edit(
        Some(EXERCISE_PHRASE),
        Some("Sandbox hot phrase."),
        Some(true),
    );
    anyhow::ensure!(editor.flush(), "the hot phrase was not saved");
    let saved = HotPhraseStore::new(services.root.hot_phrases_path.clone())
        .list()
        .iter()
        .any(|p| p.get("trigger").and_then(Value::as_str) == Some(EXERCISE_PHRASE));
    anyhow::ensure!(saved, "the hot phrase did not reach the sandbox store");
    done.push(format!("hot phrases: added {EXERCISE_PHRASE:?}"));

    let newest = services.history()?.recent(1, None, 0)?;
    let id = newest.first().context("the sandbox history is empty")?.id;
    anyhow::ensure!(
        history::delete(&services.root, id)?,
        "history entry {id} was not deleted"
    );
    anyhow::ensure!(
        services.history()?.get(id)?.is_none(),
        "history entry {id} is still in the sandbox store"
    );
    done.push(format!("history: deleted entry {id}"));

    let queue = services.remote.prompts(&services.config());
    let rows = prompts::load(queue.as_ref()).map_err(anyhow::Error::msg)?;
    let row = rows
        .iter()
        .find(|r| r.status == "pending")
        .context("no pending sandbox prompt to approve")?;
    prompts::review(queue.as_ref(), &row.id, true).map_err(anyhow::Error::msg)?;
    let after = prompts::load(queue.as_ref()).map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        after
            .iter()
            .any(|r| r.id == row.id && r.status == "approved"),
        "pending prompt {} is not approved in the sandbox queue",
        row.id
    );
    done.push(format!("prompts: approved pending prompt {}", row.id));
    Ok(done)
}

// ------------------------------------------------- simulated model work ---

/// The fixed sandbox trial transcript.
pub const SIMULATED_TRANSCRIPT: &str = "This is a simulated transcript. No audio was recorded.";

/// The sandbox's one simulated model inventory: which speech models are
/// "downloaded" and which models the simulated Ollama has. Storage,
/// downloads, setup inspection and cleanup pulls all read and write it.
pub struct SimSpeech {
    installed: std::sync::Mutex<std::collections::BTreeSet<String>>,
    ollama: std::sync::Mutex<Vec<String>>,
}

/// The simulated repo a speech model is stored in (native layout: every
/// Whisper size is one GGML file in the whisper.cpp repo).
pub fn sim_repo(model_id: &str) -> &'static str {
    if model_id.starts_with("parakeet") {
        lf_stt_repo::PARAKEET_ONNX
    } else {
        lf_stt_repo::WHISPER_CPP
    }
}

/// Repo names of `lf_stt::model_storage` (not linked in default builds).
pub mod lf_stt_repo {
    pub const WHISPER_CPP: &str = "ggerganov/whisper.cpp";
    pub const PARAKEET_ONNX: &str = "csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8";
}

impl SimSpeech {
    /// Base and Small Whisper plus Parakeet (simulated as leased by a
    /// running LocalFlow); Ollama has two cleanup models.
    pub fn seeded() -> std::sync::Arc<Self> {
        std::sync::Arc::new(SimSpeech {
            installed: std::sync::Mutex::new(
                ["fw-base", "fw-small", "parakeet-v2"]
                    .into_iter()
                    .map(String::from)
                    .collect(),
            ),
            ollama: std::sync::Mutex::new(vec!["llama3.2:3b".into(), "qwen3:4b".into()]),
        })
    }

    fn installed(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeSet<String>> {
        self.installed.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn is_installed(&self, model_id: &str) -> bool {
        self.installed().contains(model_id)
    }

    pub fn ollama_models(&self) -> Vec<String> {
        self.ollama
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// Sandbox speech storage over [`SimSpeech`]: one row per simulated repo,
/// with the same protection rules as the real cache (selected in the
/// sandbox config, or leased by a "running" LocalFlow).
pub struct SimStorage {
    state: std::sync::Arc<SimSpeech>,
}

impl SimStorage {
    pub fn new(state: std::sync::Arc<SimSpeech>) -> Self {
        SimStorage { state }
    }

    fn status(repo: &str, config: &Config) -> &'static str {
        if repo == lf_stt_repo::PARAKEET_ONNX {
            return "In use"; // simulated lease of a running LocalFlow
        }
        let backend = config.field_str("stt_backend").unwrap_or("");
        let selected = match repo {
            lf_stt_repo::WHISPER_CPP => !matches!(backend, "parakeet" | "parakeet-onnx"),
            _ => false,
        };
        if selected {
            "Selected"
        } else {
            ""
        }
    }
}

impl crate::controllers::model_storage::SpeechStorage for SimStorage {
    fn inventory(
        &self,
        config: &Config,
    ) -> std::result::Result<crate::controllers::model_storage::Inventory, String> {
        let installed = self.state.installed().clone();
        let mut models = Vec::new();
        for repo in [lf_stt_repo::WHISPER_CPP, lf_stt_repo::PARAKEET_ONNX] {
            let ids: Vec<&String> = installed.iter().filter(|id| sim_repo(id) == repo).collect();
            if ids.is_empty() {
                continue;
            }
            let size: u64 = ids
                .iter()
                .filter_map(|id| lf_ui::setup_flow::stt_model_by_id(id))
                .map(|m| u64::from(m.size_mb) * 1_000_000)
                .sum();
            let label = if repo == lf_stt_repo::WHISPER_CPP {
                format!(
                    "Whisper GGML (example: {})",
                    ids.iter()
                        .map(|id| id.trim_start_matches("fw-"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            } else {
                "Parakeet v2 (example)".to_string()
            };
            models.push(crate::controllers::model_storage::StoredModel {
                repo: repo.into(),
                label,
                size_bytes: size,
                status: Self::status(repo, config).into(),
            });
        }
        Ok(crate::controllers::model_storage::Inventory {
            cache_dir: None,
            models,
        })
    }

    fn delete(&self, repo: &str, config: &Config) -> std::result::Result<u64, String> {
        let inventory = self.inventory(config)?;
        let Some(model) = inventory.models.iter().find(|m| m.repo == repo) else {
            return Err("That isn't a speech model LocalFlow manages.".into());
        };
        if let Some(reason) = model.protection_reason() {
            return Err(reason.into());
        }
        self.state.installed().retain(|id| sim_repo(id) != repo);
        Ok(model.size_bytes)
    }
}

/// The fixed sandbox History retry transcript.
pub const SIMULATED_RETRY: &str = "this is a simulated retry no audio was transcribed";

/// Sandbox History retry: the real pipeline (sandbox dictionary, snippets,
/// row app context) over a fixed fake transcript, never a model. "Opening"
/// the model succeeds only while the shared [`SimSpeech`] inventory has the
/// exact configured model (like native cache-only retry, which needs that
/// one file, not merely its repo), so a missing size or a deleted repo shows
/// "not downloaded". No cleanup provider (a real one could make an HTTP
/// call) and no keys.
pub struct SimRetry {
    state: std::sync::Arc<SimSpeech>,
}

impl SimRetry {
    pub fn new(state: std::sync::Arc<SimSpeech>) -> Self {
        SimRetry { state }
    }
}

/// The simulated inventory id of the configured speech model, mirroring the
/// native factory (`lf_stt::factory::canonical_backend`, not linked here):
/// missing `stt_backend` is `auto`, missing `model_size` is `base`; every
/// Whisper alias selects the whisper.cpp GGML file of that size (the `fw-*`
/// catalog entry), Parakeet ignores the size. Unknown backends and sizes
/// outside the catalog are errors, never another model.
pub fn sim_model_id(config: &Config) -> std::result::Result<String, String> {
    let backend = config.field_str("stt_backend").unwrap_or("auto");
    let size = config.field_str("model_size").unwrap_or("base");
    let id = match backend {
        "auto" | "faster-whisper" | "mlx" | "mlx-whisper" | "whisper-cpp" => format!("fw-{size}"),
        "parakeet" | "parakeet-onnx" => "parakeet-v2".to_string(),
        _ => return Err(format!("unsupported speech provider: {backend}")),
    };
    match lf_ui::setup_flow::stt_model_by_id(&id) {
        Some(_) => Ok(id),
        None => Err(format!(
            "The selected speech model ({backend} {size}) isn't available in the sandbox example."
        )),
    }
}

struct SimRetrySpeech;

impl lf_runtime::engine::SpeechEngine for SimRetrySpeech {
    fn load(&self) -> std::result::Result<(), String> {
        Ok(())
    }
    fn transcribe(&self, _: &[f32], _: Option<&str>) -> std::result::Result<String, String> {
        Ok(SIMULATED_RETRY.into())
    }
    fn unload(&self) {}
}

impl crate::controllers::retry::RetryBackend for SimRetry {
    fn unavailable(&self) -> Option<String> {
        None
    }
    fn open(
        &self,
        config: &Config,
    ) -> std::result::Result<std::sync::Arc<dyn lf_runtime::engine::SpeechEngine>, String> {
        if !self.state.is_installed(&sim_model_id(config)?) {
            return Err("The selected speech model isn't downloaded (sandbox example). Download it in Models & keys, then try again.".into());
        }
        Ok(std::sync::Arc::new(SimRetrySpeech))
    }
    fn llm(&self, _: &Config) -> Option<Box<dyn lf_providers::LlmProvider>> {
        None
    }
    fn simulated(&self) -> bool {
        true
    }
}

/// Setup/model operations for the sandbox over [`SimSpeech`]: downloads tick
/// to 100 % over about three seconds, a pull adds its model to the simulated
/// Ollama, the trial returns a fixed transcript. No audio, network, model
/// files or keyring.
pub struct SimSetupBackend {
    state: std::sync::Arc<SimSpeech>,
    step: std::time::Duration,
}

impl SimSetupBackend {
    pub fn new() -> Self {
        Self::with_state(SimSpeech::seeded(), std::time::Duration::from_millis(100))
    }

    pub fn with_step(step: std::time::Duration) -> Self {
        Self::with_state(SimSpeech::seeded(), step)
    }

    pub fn with_state(state: std::sync::Arc<SimSpeech>, step: std::time::Duration) -> Self {
        SimSetupBackend { state, step }
    }

    fn tick(
        &self,
        total: u64,
        status: &str,
        progress: &lf_ui::setup_backend::ProgressCallback,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> std::result::Result<(), String> {
        const STEPS: u64 = 30;
        for i in 0..=STEPS {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                return Err("Download cancelled.".into());
            }
            progress(lf_ui::setup_backend::DownloadProgress {
                done: total * i / STEPS,
                total,
                status: status.into(),
            });
            std::thread::sleep(self.step);
        }
        Ok(())
    }
}

impl Default for SimSetupBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl lf_ui::setup_backend::SetupModelBackend for SimSetupBackend {
    fn speech_format(&self, _: &lf_ui::setup_flow::SttModel) -> Option<&'static str> {
        Some("Simulated")
    }
    fn speech_downloads(
        &self,
        models: &[lf_ui::setup_flow::SttModel],
    ) -> std::result::Result<std::collections::HashMap<String, bool>, String> {
        Ok(models
            .iter()
            .map(|m| (m.id.clone(), self.state.is_installed(&m.id)))
            .collect())
    }
    fn download_speech(
        &self,
        model: &lf_ui::setup_flow::SttModel,
        progress: lf_ui::setup_backend::ProgressCallback,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> std::result::Result<(), String> {
        self.tick(
            u64::from(model.size_mb) * 1_000_000,
            "Simulated download",
            &progress,
            &cancel,
        )?;
        self.state.installed().insert(model.id.clone());
        Ok(())
    }
    fn ollama(&self, _: &str) -> std::result::Result<(bool, Vec<String>), String> {
        Ok((true, self.state.ollama_models()))
    }
    fn pull_cleanup(
        &self,
        _: &str,
        model_id: &str,
        progress: lf_ui::setup_backend::ProgressCallback,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> std::result::Result<(), String> {
        self.tick(2_000_000_000, "Simulated pull", &progress, &cancel)?;
        let mut ollama = self.state.ollama.lock().unwrap_or_else(|p| p.into_inner());
        if !ollama.iter().any(|m| m == model_id) {
            ollama.push(model_id.to_string());
        }
        Ok(())
    }
    fn try_dictation(
        &self,
        model: &lf_ui::setup_flow::SttModel,
        device: Option<&str>,
        language: Option<&str>,
    ) -> std::result::Result<String, String> {
        self.try_dictation_cancellable(
            model,
            device,
            language,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
    }
    fn try_dictation_cancellable(
        &self,
        _: &lf_ui::setup_flow::SttModel,
        _: Option<&str>,
        _: Option<&str>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> std::result::Result<String, String> {
        for _ in 0..15 {
            lf_providers::transfer::check_cancel(&cancel)?;
            std::thread::sleep(self.step);
        }
        Ok(SIMULATED_TRANSCRIPT.into())
    }
}

/// A canned microphone level curve; the real microphone is never opened.
pub struct SimMeterInput;

struct SimMeter(std::time::Instant);

impl crate::controllers::setup_flow::Meter for SimMeter {
    fn level(&self) -> f64 {
        let t = self.0.elapsed().as_secs_f64();
        0.05 + 0.04 * (t * 3.0).sin().abs()
    }
    fn stop(&mut self) {}
}

impl crate::controllers::setup_flow::MeterInput for SimMeterInput {
    fn open(
        &self,
        _: Option<&str>,
    ) -> std::result::Result<Box<dyn crate::controllers::setup_flow::Meter>, String> {
        Ok(Box::new(SimMeter(std::time::Instant::now())))
    }
}

/// The sandbox machine: Linux, two example microphones, Ollama installed.
pub fn setup_platform() -> lf_ui::setup_flow::FakeSetupPlatform {
    lf_ui::setup_flow::FakeSetupPlatform {
        system: if cfg!(target_os = "macos") {
            "Darwin"
        } else {
            "Linux"
        }
        .into(),
        machine: std::env::consts::ARCH.into(),
        commands: vec!["ffmpeg".into(), "xdotool".into()],
        ollama_installed: true,
        ollama_running: Some(true),
        input_devices: Ok(vec![
            "Built-in Microphone (example)".into(),
            "USB Headset (example)".into(),
        ]),
        hardware: Ok(lf_ui::setup_flow::HardwareInfo::new(
            if cfg!(target_os = "macos") {
                "Darwin"
            } else {
                "Linux"
            },
            std::env::consts::ARCH,
            16.0,
            false,
            120.0,
        )),
        ..lf_ui::setup_flow::FakeSetupPlatform::default()
    }
}

// ------------------------------------------------------------ Ask tools ---

/// Sandbox Ask: fake LocalFlow tools over the sandbox history and prompt
/// queue, a scripted model, and `remote::sandbox_answer` as the fallback.
/// No process is started and nothing leaves the process.
pub struct SandboxAgent {
    root: DataRoot,
    prompts: std::sync::Arc<FakePromptQueue>,
    latency: std::time::Duration,
}

impl SandboxAgent {
    /// `latency` paces each fake tool call and model step so the UI's
    /// thinking and running states are visible; tests pass zero.
    pub fn new(
        root: DataRoot,
        prompts: std::sync::Arc<FakePromptQueue>,
        latency: std::time::Duration,
    ) -> Self {
        SandboxAgent {
            root,
            prompts,
            latency,
        }
    }
}

/// Wait `latency` in short slices; `false` when stopped meanwhile.
fn pause(latency: std::time::Duration, cancel: &std::sync::atomic::AtomicBool) -> bool {
    use std::sync::atomic::Ordering;
    let end = std::time::Instant::now() + latency;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let left = end.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(crate::controllers::mcp_client::POLL));
    }
}

impl crate::controllers::ask_agent::AgentBackend for SandboxAgent {
    fn simulated(&self) -> bool {
        true
    }

    fn connect(
        &self,
        cancel: &std::sync::atomic::AtomicBool,
        deadline: Option<std::time::Instant>,
    ) -> Result<
        Box<dyn crate::controllers::mcp_client::ToolTransport>,
        crate::controllers::mcp_client::McpError,
    > {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(crate::controllers::mcp_client::McpError::Cancelled);
        }
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            return Err(crate::controllers::mcp_client::McpError::Deadline);
        }
        Ok(Box::new(SandboxTools {
            root: self.root.clone(),
            prompts: self.prompts.clone(),
            latency: self.latency,
        }))
    }

    fn model(&self, _: i64) -> Result<Box<dyn lf_providers::LlmProvider>, String> {
        Ok(Box::new(SandboxModel {
            latency: self.latency,
        }))
    }

    /// In-process and instant: the budget has nothing to bound.
    fn recall(
        &self,
        question: &str,
        _: &str,
        _: std::time::Duration,
    ) -> lf_ui::backends::ask::Turn {
        crate::remote::sandbox_answer(&self.root, question)
    }
}

/// The four LocalFlow tools over sandbox data, with the Python server's
/// result shapes (`localflow/mcp_server.py`).
pub struct SandboxTools {
    root: DataRoot,
    prompts: std::sync::Arc<FakePromptQueue>,
    latency: std::time::Duration,
}

fn local_when(ts: f64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .map(|t| t.format("%Y-%m-%dT%H:%M:%S").to_string())
        .unwrap_or_default()
}

/// `recall.parse_since` (+ one day for a named `until` day), epoch seconds.
fn bound(value: Option<&str>, end: bool, now: chrono::DateTime<chrono::Local>) -> Option<f64> {
    use chrono::TimeZone;
    let text = value?.trim().to_ascii_lowercase();
    if text.is_empty() {
        return None;
    }
    let midnight = |d: chrono::NaiveDate| {
        chrono::Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0)?)
            .earliest()
            .map(|t| t.timestamp() as f64)
    };
    let day = 86_400.0;
    let extra = if end { day } else { 0.0 };
    match text.as_str() {
        "today" => midnight(now.date_naive()).map(|t| t + extra),
        "yesterday" => midnight(now.date_naive()).map(|t| t - day + extra),
        _ => {
            if let Some(n) = text
                .strip_suffix('d')
                .and_then(|n| n.trim().parse::<f64>().ok())
            {
                Some(now.timestamp() as f64 - n * day)
            } else {
                chrono::NaiveDate::parse_from_str(&text, "%Y-%m-%d")
                    .ok()
                    .and_then(midnight)
                    .map(|t| t + extra)
            }
        }
    }
}

/// Question words that carry no topic. Short topic words ("git", "k8s",
/// "CI") are kept: a length cut-off would drop them.
const STOPWORDS: &[&str] = &[
    "a", "about", "all", "an", "and", "any", "are", "as", "at", "be", "by", "can", "could", "did",
    "do", "does", "for", "from", "had", "has", "have", "how", "i", "if", "in", "is", "it", "me",
    "my", "of", "on", "or", "our", "said", "say", "so", "tell", "that", "the", "this", "to", "us",
    "was", "we", "were", "what", "when", "where", "which", "who", "why", "with", "you", "your",
];

/// Lowercase alphanumeric tokens of `text`, in order.
fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
}

/// Distinct search terms of a query or question: every token except
/// stopwords, whatever its length.
fn words(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for token in tokens(text) {
        if !STOPWORDS.contains(&token.as_str()) && !out.contains(&token) {
            out.push(token);
        }
    }
    out
}

/// Words that frame a question about the history ("what have I dictated
/// about …") rather than name its topic.
const FRAMING: &[&str] = &[
    "dictate",
    "dictated",
    "dictation",
    "dictations",
    "mention",
    "mentioned",
    "talk",
    "talked",
    "write",
    "wrote",
    "written",
];

/// Conversational qualifiers that can trail a question ("… about again?")
/// without naming its topic.
const QUALIFIERS: &[&str] = &["again", "anyway", "exactly", "now", "please", "then"];

/// `terms` without any of `drop`, or `None` when nothing would be left.
fn without(terms: &[String], drop: &[&str]) -> Option<Vec<String>> {
    let kept: Vec<String> = terms
        .iter()
        .filter(|w| !drop.contains(&w.as_str()))
        .cloned()
        .collect();
    (!kept.is_empty()).then_some(kept)
}

/// The sandbox model's search terms for a question: its topic only.
///
/// - "about" followed by a topic: what follows the last "about", minus
///   trailing qualifiers ("What have I dictated about the launch?" ->
///   `launch`).
/// - "about" followed only by qualifiers: the words before it, minus framing
///   words and qualifiers, when there are any ("What was that Kubernetes
///   dictation about again?" -> `kubernetes`); otherwise the qualifier is
///   itself the topic ("What did I write about again?" -> `again`).
/// - No "about": the question minus framing words, unless that leaves
///   nothing ("dictated" -> `dictated`). Qualifiers are dropped only in the
///   "about …" phrase, never elsewhere.
fn question_topic(question: &str) -> Vec<String> {
    // One tokenization pass; `terms` is `words` over a slice of it.
    let all: Vec<String> = tokens(question).collect();
    let terms = |slice: &[String]| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for token in slice {
            if !STOPWORDS.contains(&token.as_str()) && !out.contains(token) {
                out.push(token.clone());
            }
        }
        out
    };
    if let Some(at) = all.iter().rposition(|t| t == "about") {
        let suffix = terms(&all[at + 1..]);
        if !suffix.is_empty() {
            return match without(&suffix, QUALIFIERS) {
                Some(topic) => topic,
                None => without(&terms(&all[..at]), FRAMING)
                    .and_then(|prefix| without(&prefix, QUALIFIERS))
                    .unwrap_or(suffix),
            };
        }
    }
    let whole = terms(&all);
    without(&whole, FRAMING).unwrap_or(whole)
}

/// A search query from `terms` that `ask_agent::validate_args` accepts: at
/// most `max` Unicode characters (not bytes), whole terms joined by spaces,
/// in order. A term that doesn't fit is skipped, never ending the query, so
/// later terms that fit are still searched. Only when no whole term fits at
/// all is the first one cut to `max` characters: still a prefix of the word
/// (long terms match word starts), so an explicit topic never turns into an
/// unfiltered listing. Empty only for no terms.
fn bounded_query(terms: &[String], max: usize) -> String {
    let mut query = String::new();
    let mut used = 0;
    for term in terms {
        let len = term.chars().count();
        let extra = if used == 0 { len } else { len + 1 };
        if len == 0 || used + extra > max {
            continue;
        }
        if used > 0 {
            query.push(' ');
        }
        query.push_str(term);
        used += extra;
    }
    if query.is_empty() {
        if let Some(first) = terms.iter().find(|t| !t.is_empty()) {
            query = first.chars().take(max).collect();
        }
    }
    query
}

/// Japanese (kanji/Han, hiragana, katakana) is written without spaces, so a
/// whole sentence is one token and word rules can't find a phrase in it.
fn unspaced_script(c: char) -> bool {
    matches!(c as u32,
        0x3005 | 0x3007            // 々 〇
        | 0x3040..=0x309F          // hiragana
        | 0x30A0..=0x30FF          // katakana
        | 0x31F0..=0x31FF          // katakana phonetic extensions
        | 0xFF66..=0xFF9F          // half-width katakana
        | 0x3400..=0x4DBF          // CJK extension A
        | 0x4E00..=0x9FFF          // CJK unified ideographs
        | 0xF900..=0xFAFF          // CJK compatibility ideographs
        | 0x20000..=0x3134F // CJK extensions B–G
    )
}

/// How many `terms` occur in `text`. Terms under four characters must be a
/// whole word (so "git" doesn't match inside "digital"); longer ones may
/// start a word, as the old substring match allowed ("fail" -> "failed").
/// A term with Japanese characters may occur anywhere inside a word
/// ("午後二時" in "会議は木曜日の午後二時に移動しました").
fn term_score(terms: &[String], text: &str) -> usize {
    let present: Vec<String> = tokens(text).collect();
    terms
        .iter()
        .filter(|t| {
            present.iter().any(|w| {
                if t.chars().any(unspaced_script) {
                    w.contains(t.as_str())
                } else if t.chars().count() < 4 {
                    w == *t
                } else {
                    w.starts_with(t.as_str())
                }
            })
        })
        .count()
}

/// The query's terms, or `None` for no query at all (list the latest). A
/// non-empty query with no searchable term gives an empty term list, which
/// matches nothing rather than everything.
fn query_terms(query: Option<&str>) -> Option<Vec<String>> {
    query.map(str::trim).filter(|q| !q.is_empty()).map(words)
}

fn tool_error(text: impl Into<String>) -> crate::controllers::mcp_client::ToolOutput {
    crate::controllers::mcp_client::ToolOutput {
        is_error: true,
        text: text.into(),
        structured: None,
    }
}

fn tool_ok(data: Value) -> crate::controllers::mcp_client::ToolOutput {
    crate::controllers::mcp_client::ToolOutput {
        is_error: false,
        text: data.to_string(),
        structured: Some(data),
    }
}

impl SandboxTools {
    fn search_dictations(&self, args: &Value) -> crate::controllers::mcp_client::ToolOutput {
        let s = |k: &str| args.get(k).and_then(Value::as_str);
        let limit = args
            .get("limit")
            .and_then(Value::as_i64)
            .unwrap_or(10)
            .clamp(1, 50) as usize;
        let now = chrono::Local::now();
        let (since, until) = (bound(s("since"), false, now), bound(s("until"), true, now));
        let Ok(store) = open_history(&self.root) else {
            return tool_error("cannot read dictation history");
        };
        let entries = store.recent(200, None, 0).unwrap_or_default();
        let app = s("app")
            .map(str::to_lowercase)
            .filter(|a| !a.trim().is_empty());
        let terms = query_terms(s("query"));
        let mut hits: Vec<(usize, &lf_core::history::HistoryEntry)> = entries
            .iter()
            .filter(|e| e.status == "ok")
            .filter(|e| since.is_none_or(|t| e.ts >= t) && until.is_none_or(|t| e.ts < t))
            .filter(|e| s("kind").is_none_or(|k| e.kind == k))
            .filter(|e| {
                app.as_ref().is_none_or(|a| {
                    e.app_name
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(a.as_str())
                        || e.bundle_id.as_deref() == s("app")
                })
            })
            .map(|e| match &terms {
                None => (0, e),
                Some(terms) => (
                    term_score(terms, &format!("{} {}", e.final_text, e.raw_text)),
                    e,
                ),
            })
            .filter(|(score, _)| terms.is_none() || *score > 0)
            .collect();
        // Stable: newest first within one score.
        hits.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        let results: Vec<Value> = hits
            .iter()
            .take(limit)
            .map(|(_, e)| {
                json!({
                    "id": e.id,
                    "when": local_when(e.ts),
                    "kind": e.kind,
                    "app": e.app_name.clone().unwrap_or_default(),
                    "category": e.category.clone().unwrap_or_default(),
                    "words": e.words,
                    "duration_s": e.duration_s,
                    "snippet": e.final_text.chars().take(160).collect::<String>(),
                })
            })
            .collect();
        tool_ok(json!({
            "count": results.len(),
            "match": if terms.is_none() { "recent" } else { "ranked" },
            "results": results,
        }))
    }

    fn get_dictation(&self, args: &Value) -> crate::controllers::mcp_client::ToolOutput {
        let id = args.get("id").and_then(Value::as_i64).unwrap_or_default();
        let entry = open_history(&self.root)
            .ok()
            .and_then(|store| store.get(id).ok().flatten());
        let Some(e) = entry else {
            return tool_error(format!("no dictation with id {id}"));
        };
        tool_ok(json!({
            "id": e.id,
            "when": local_when(e.ts),
            "kind": e.kind,
            "status": e.status,
            "app": e.app_name.clone().unwrap_or_default(),
            "bundle_id": e.bundle_id.clone().unwrap_or_default(),
            "category": e.category.clone().unwrap_or_default(),
            "words": e.words,
            "duration_s": e.duration_s,
            "final_text": e.final_text,
            "raw_text": e.raw_text,
            "truncated": false,
        }))
    }

    fn prompt_fields(entry: &Map<String, Value>) -> Value {
        let f = |k: &str| {
            entry
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        json!({
            "id": f("id"),
            "title": f("title"),
            "meeting": f("meeting"),
            "status": f("status"),
            "created": f("created"),
        })
    }

    fn search_prompts(&self, args: &Value) -> crate::controllers::mcp_client::ToolOutput {
        let status = args.get("status").and_then(Value::as_str);
        let limit = args
            .get("limit")
            .and_then(Value::as_i64)
            .unwrap_or(10)
            .clamp(1, 50) as usize;
        let terms = query_terms(args.get("query").and_then(Value::as_str));
        let Ok(mut entries) = self.prompts.list(status) else {
            return tool_error("prompt queue unreadable");
        };
        entries.sort_by(|a, b| {
            let created = |e: &Map<String, Value>| {
                e.get("created")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            created(b).cmp(&created(a))
        });
        let results: Vec<Value> = entries
            .iter()
            .filter(|e| {
                let Some(terms) = &terms else {
                    return true;
                };
                let text = format!(
                    "{} {}",
                    e.get("title").and_then(Value::as_str).unwrap_or(""),
                    e.get("meeting").and_then(Value::as_str).unwrap_or("")
                );
                term_score(terms, &text) > 0
            })
            .take(limit)
            .map(|e| {
                let mut v = Self::prompt_fields(e);
                v["snippet"] = json!("Example prompt.");
                v
            })
            .collect();
        tool_ok(json!({
            "count": results.len(),
            "match": if terms.is_none() { "recent" } else { "ranked" },
            "results": results,
        }))
    }

    fn get_prompt(&self, args: &Value) -> crate::controllers::mcp_client::ToolOutput {
        let id = args.get("id").and_then(Value::as_str).unwrap_or("");
        match self.prompts.get(id) {
            Ok(Some(entry)) => {
                let mut v = Self::prompt_fields(&entry);
                v["body"] = json!("Example prompt.");
                v["error"] = json!(entry.get("error").and_then(Value::as_str).unwrap_or(""));
                v["truncated"] = json!(false);
                tool_ok(v)
            }
            _ => tool_error(format!("no work prompt with id {id:?}")),
        }
    }
}

impl crate::controllers::mcp_client::ToolTransport for SandboxTools {
    fn tools(&self) -> Vec<String> {
        crate::controllers::mcp_client::ALLOWED_TOOLS
            .iter()
            .map(|t| t.to_string())
            .collect()
    }

    fn call(
        &mut self,
        name: &str,
        arguments: &Value,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<crate::controllers::mcp_client::ToolOutput, crate::controllers::mcp_client::McpError>
    {
        self.paced_call(name, arguments, cancel, None)
    }

    fn call_until(
        &mut self,
        name: &str,
        arguments: &Value,
        cancel: &std::sync::atomic::AtomicBool,
        deadline: std::time::Instant,
    ) -> Result<crate::controllers::mcp_client::ToolOutput, crate::controllers::mcp_client::McpError>
    {
        self.paced_call(name, arguments, cancel, Some(deadline))
    }
}

impl SandboxTools {
    /// The UI pacing never waits past the Ask run's `deadline`.
    fn paced_call(
        &mut self,
        name: &str,
        arguments: &Value,
        cancel: &std::sync::atomic::AtomicBool,
        deadline: Option<std::time::Instant>,
    ) -> Result<crate::controllers::mcp_client::ToolOutput, crate::controllers::mcp_client::McpError>
    {
        use crate::controllers::mcp_client::McpError;
        let pace = match deadline {
            Some(d) => self
                .latency
                .min(d.saturating_duration_since(std::time::Instant::now())),
            None => self.latency,
        };
        if !pause(pace, cancel) {
            return Err(McpError::Cancelled);
        }
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            return Err(McpError::Deadline);
        }
        Ok(match name {
            "search_dictations" => self.search_dictations(arguments),
            "get_dictation" => self.get_dictation(arguments),
            "search_prompts" => self.search_prompts(arguments),
            "get_prompt" => self.get_prompt(arguments),
            _ => return Err(McpError::UnknownTool),
        })
    }
}

/// A scripted model that reads only its prompt, like a real one: search,
/// open the best dictation, answer from it (or list matching work prompts).
pub struct SandboxModel {
    latency: std::time::Duration,
}

impl lf_providers::LlmProvider for SandboxModel {
    fn id(&self) -> &'static str {
        "sandbox"
    }

    fn offline(&self) -> bool {
        true
    }

    fn complete(
        &self,
        system: &str,
        user: &str,
        _: Option<i64>,
    ) -> Result<String, lf_providers::ProviderError> {
        use crate::controllers::ask_agent::{prompt_question, prompt_results};
        // UI pacing only (never in tests): a model step takes a moment.
        std::thread::sleep(self.latency);
        let question = prompt_question(user).unwrap_or("").to_lowercase();
        let results = prompt_results(user);
        let final_only = system.contains("can't call any more tools");
        let no_answer = json!({"answer": lf_ui::backends::ask::NO_ANSWER}).to_string();
        let parse = |body: &str| serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
        let wants_prompts = ["prompt", "next step", "task"]
            .iter()
            .any(|w| question.contains(w))
            && system.contains("search_prompts");
        let Some((name, status, body)) = results.last() else {
            if final_only {
                return Ok(no_answer);
            }
            // Short topic words ("git") stay in the query. A question with
            // no topic word at all lists the latest dictations (no query).
            // A detailed question is cut to what the tool accepts.
            let query = bounded_query(
                &question_topic(&question),
                crate::controllers::ask_agent::MAX_ARG_CHARS,
            );
            let tool = if wants_prompts {
                "search_prompts"
            } else {
                "search_dictations"
            };
            let arguments = if wants_prompts {
                json!({})
            } else if query.is_empty() {
                json!({"limit": 5})
            } else {
                json!({"query": query, "limit": 5})
            };
            return Ok(json!({"tool": tool, "arguments": arguments}).to_string());
        };
        let data = parse(body);
        let reply = match (name.as_str(), status.as_str()) {
            ("search_dictations", "ok") => {
                match data.pointer("/results/0/id").and_then(Value::as_i64) {
                    Some(id) if !final_only => {
                        json!({"tool": "get_dictation", "arguments": {"id": id}})
                    }
                    _ => return Ok(no_answer),
                }
            }
            ("get_dictation", "ok") => {
                let text: String = data["final_text"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect();
                json!({
                    "answer": format!(
                        "Sandbox answer (scripted model, no network): your {} dictation says “{}”",
                        data["app"].as_str().unwrap_or("example"),
                        text
                    ),
                    "sources": [data["id"]],
                })
            }
            ("search_prompts", "ok") => {
                let titles: Vec<&str> = data["results"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r["title"].as_str())
                    .collect();
                if titles.is_empty() {
                    return Ok(no_answer);
                }
                json!({"answer": format!(
                    "Sandbox answer (scripted model, no network): your example work prompts are: {}.",
                    titles.join("; ")
                )})
            }
            _ => return Ok(no_answer),
        };
        Ok(reply.to_string())
    }

    fn list_models(&self) -> Result<Vec<lf_providers::ModelInfo>, lf_providers::ProviderError> {
        Ok(Vec::new())
    }

    fn healthcheck(&self) -> lf_providers::Health {
        lf_providers::Health::ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The temp dir in the same style `persisted_clip_path` stores clips in.
    fn temp_base(dir: &tempfile::TempDir) -> std::path::PathBuf {
        if cfg!(windows) {
            dir.path().to_path_buf()
        } else {
            fs::canonicalize(dir.path()).unwrap()
        }
    }

    #[test]
    fn sandbox_retry_is_a_labelled_simulation_over_sandbox_data_only() {
        use crate::controllers::model_storage::SpeechStorage;
        use crate::controllers::retry::{retry_entry, RetryBackend, RetryKey};
        let dir = tempfile::tempdir().unwrap();
        let base = temp_base(&dir);
        let root = prepare(&base.join("sb"), false).unwrap();
        let services = crate::data::Services::open(root.clone()).unwrap();
        assert!(services.retry.simulated());
        assert!(services.retry.unavailable().is_none());
        assert!(
            services.retry.llm(&services.config()).is_none(),
            "no provider"
        );
        let history = open_history(&root).unwrap();
        let failed: Vec<_> = history
            .recent(100, None, 0)
            .unwrap()
            .into_iter()
            .filter(|e| e.status == "failed")
            .collect();
        assert_eq!(failed.len(), 1);
        let entry = failed[0].clone();
        let shown =
            crate::controllers::retry::view(&entry, services.retry.as_ref(), &services.retry_jobs);
        assert!(shown.blocked.is_none());
        assert!(shown
            .note
            .starts_with(crate::controllers::retry::SIMULATED_NOTE));
        let clip = std::path::PathBuf::from(entry.audio_path.clone().unwrap());
        assert!(clip.starts_with(&root.clips_dir) && clip.is_file());
        let updated = retry_entry(
            &root,
            &services.config(),
            RetryKey::of(&entry),
            services.retry.as_ref(),
        )
        .unwrap();
        assert_eq!(updated.status, "ok");
        assert_eq!(updated.raw_text, SIMULATED_RETRY);
        assert_eq!(updated.id, entry.id);
        assert_eq!(history.recent(100, None, 0).unwrap().len(), HISTORY.len());

        // Without the simulated Whisper model, "open" refuses honestly.
        let sim = SimSpeech::seeded();
        let storage = std::sync::Arc::new(SimStorage::new(sim.clone()));
        let mut other = Config::defaults_for(false);
        other.set_field("stt_backend", json!("parakeet"));
        storage.delete(lf_stt_repo::WHISPER_CPP, &other).unwrap();
        let retry = SimRetry::new(sim);
        let error = retry.open(&services.config()).err().unwrap();
        assert!(error.contains("isn't downloaded"), "{error}");
        for path in services.root.all_paths() {
            assert!(path.starts_with(&base));
        }
    }

    /// Retry needs the exact configured model in the one shared simulated
    /// inventory (as native cache-only retry needs that exact file), not
    /// merely another size in the same whisper.cpp repo.
    #[test]
    fn sandbox_retry_requires_the_exact_selected_simulated_model() {
        use crate::controllers::retry::{retry_entry, RetryKey};
        let dir = tempfile::tempdir().unwrap();
        let root = prepare(&dir.path().join("sb"), false).unwrap();
        let services = crate::data::Services::open(root.clone()).unwrap();
        let failed = open_history(&root)
            .unwrap()
            .recent(100, None, 0)
            .unwrap()
            .into_iter()
            .find(|e| e.status == "failed")
            .unwrap();
        let entry = || failed.clone();
        let with = |backend: &str, size: &str| {
            let mut c = services.config();
            c.set_field("stt_backend", json!(backend));
            c.set_field("model_size", json!(size));
            c
        };
        let not_downloaded = |config: &Config| {
            let error = services.retry.open(config).err().unwrap();
            assert!(error.contains("isn't downloaded"), "{error}");
        };

        // Seeded fw-base: retry works.
        let base = with("faster-whisper", "base");
        services.retry.open(&base).unwrap();
        let first = entry();
        let updated =
            retry_entry(&root, &base, RetryKey::of(&first), services.retry.as_ref()).unwrap();
        assert_eq!(updated.status, "ok");
        assert_eq!(updated.raw_text, SIMULATED_RETRY);

        // Medium/large are not downloaded although the whisper.cpp repo is
        // (fw-base, fw-small), under every supported Whisper alias.
        assert!(services
            .storage
            .inventory(&base)
            .unwrap()
            .models
            .iter()
            .any(|m| m.repo == lf_stt_repo::WHISPER_CPP));
        for backend in [
            "faster-whisper",
            "auto",
            "mlx",
            "mlx-whisper",
            "whisper-cpp",
        ] {
            not_downloaded(&with(backend, "medium"));
            not_downloaded(&with(backend, "large-v3"));
        }
        let medium = with("faster-whisper", "medium");
        let err = retry_entry(
            &root,
            &medium,
            RetryKey::of(&entry()),
            services.retry.as_ref(),
        );
        assert!(err.is_err());

        // Simulated download (through the services' own setup backend, so
        // the same shared inventory) of exactly medium makes medium work.
        let model = lf_ui::setup_flow::stt_model_by_id("fw-medium").unwrap();
        services
            .setup
            .models
            .download_speech(
                &model,
                Box::new(|_| {}),
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .unwrap();
        services.retry.open(&medium).unwrap();
        services.retry.open(&with("mlx-whisper", "medium")).unwrap();
        not_downloaded(&with("faster-whisper", "large-v3"));
        let again = retry_entry(
            &root,
            &medium,
            RetryKey::of(&entry()),
            services.retry.as_ref(),
        )
        .unwrap();
        assert_eq!(again.status, "ok");

        // Deleting the simulated repo (with Parakeet selected, so Whisper
        // isn't protected) makes it fail again.
        services
            .storage
            .delete(lf_stt_repo::WHISPER_CPP, &with("parakeet", "base"))
            .unwrap();
        not_downloaded(&medium);
        not_downloaded(&base);
        // Parakeet ignores the size and is still seeded.
        services.retry.open(&with("parakeet", "medium")).unwrap();
        services.retry.open(&with("parakeet-onnx", "base")).unwrap();

        // Unsupported or unknown identities never select another model.
        for (backend, size) in [
            ("whisper", "base"),
            ("openai", "base"),
            ("", "base"),
            ("faster-whisper", "huge"),
            ("faster-whisper", ""),
        ] {
            let error = services.retry.open(&with(backend, size)).err().unwrap();
            assert!(
                !error.contains("isn't downloaded"),
                "{backend}/{size}: {error}"
            );
        }
        assert_eq!(
            sim_model_id(&with("whisper", "base")).unwrap_err(),
            "unsupported speech provider: whisper"
        );
        // Missing fields follow the native defaults (auto, base).
        let mut bare = Config::defaults_for(false);
        bare.fields.remove("stt_backend");
        bare.fields.remove("model_size");
        assert_eq!(sim_model_id(&bare).unwrap(), "fw-base");
        assert!(services.retry.llm(&medium).is_none());
        assert!(services.retry.simulated());
    }

    /// A relative `--sandbox` root must not leave cwd-relative clip paths in
    /// SQLite: relaunching the same root by absolute path (as from another
    /// working directory) still finds the clip and retries as a simulation.
    #[cfg(unix)]
    #[test]
    fn relative_sandbox_root_saves_a_clip_path_that_survives_relaunch() {
        use crate::controllers::retry::{retry_entry, RetryKey};
        let dir = tempfile::tempdir().unwrap();
        let absolute = fs::canonicalize(dir.path()).unwrap().join("sb");
        // Reach the tempdir relative to the current cwd, without chdir.
        let cwd = std::env::current_dir().unwrap();
        let mut relative = std::path::PathBuf::new();
        for _ in cwd.components().skip(1) {
            relative.push("..");
        }
        relative.push(absolute.strip_prefix("/").unwrap());
        assert!(relative.is_relative());

        let seeded = prepare(&relative, false).unwrap();
        assert!(seeded.clips_dir.is_relative(), "root kept as given");
        let stored = open_history(&seeded)
            .unwrap()
            .recent(100, None, 0)
            .unwrap()
            .into_iter()
            .find(|e| e.status == "failed")
            .unwrap();
        let clip = std::path::PathBuf::from(stored.audio_path.clone().unwrap());
        assert!(clip.is_absolute(), "{}", clip.display());
        assert!(clip.starts_with(absolute.join("")), "{}", clip.display());

        // Relaunch by absolute path: no reseed, same row, real loader.
        let root = prepare(&absolute, false).unwrap();
        let services = crate::data::Services::open(root.clone()).unwrap();
        assert!(services.retry.simulated());
        let entry = open_history(&root)
            .unwrap()
            .get(stored.id)
            .unwrap()
            .unwrap();
        assert_eq!(entry.audio_path, stored.audio_path);
        let updated = retry_entry(
            &root,
            &services.config(),
            RetryKey::of(&entry),
            services.retry.as_ref(),
        )
        .unwrap();
        assert_eq!(updated.status, "ok");
        assert_eq!(updated.raw_text, SIMULATED_RETRY);
        for path in services.root.all_paths() {
            assert!(path.starts_with(&absolute));
        }
    }

    /// An ordinary absolute root (e.g. `C:\scratch\sandbox` on Windows) saves
    /// a clip path in the root's own prefix style, so HistoryStore's real
    /// containment checks load, retry and then delete it. A verbatim
    /// `\\?\C:\...` clip under a non-verbatim `C:\...` root fails all three.
    #[test]
    fn absolute_sandbox_root_clip_loads_retries_and_deletes() {
        use crate::controllers::retry::{retry_entry, RetryKey};
        use std::path::{Component, Path};
        // The Windows prefix of a path, if any, and whether it is verbatim;
        // no assumption about which style the test temp dir uses.
        fn verbatim(path: &Path) -> Option<bool> {
            match path.components().next() {
                Some(Component::Prefix(p)) => Some(p.kind().is_verbatim()),
                _ => None,
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let base = std::path::absolute(dir.path().join("sb")).unwrap();
        let root = prepare(&base, false).unwrap();
        assert!(root.clips_dir.is_absolute());
        let history = open_history(&root).unwrap();
        let entry = history
            .recent(100, None, 0)
            .unwrap()
            .into_iter()
            .find(|e| e.status == "failed")
            .unwrap();
        let clip = std::path::PathBuf::from(entry.audio_path.clone().unwrap());
        assert!(clip.is_absolute(), "{}", clip.display());
        assert_eq!(
            verbatim(&clip),
            verbatim(&root.clips_dir),
            "{} vs {}",
            clip.display(),
            root.clips_dir.display()
        );
        assert!(clip.is_file(), "{}", clip.display());

        // The store's own loader accepts it as inside the clips directory.
        let (samples, rate) = history.load_clip(&clip).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(samples.len(), 24_000);

        // Retry (which loads through the store) works as a simulation.
        let services = crate::data::Services::open(root.clone()).unwrap();
        let updated = retry_entry(
            &root,
            &services.config(),
            RetryKey::of(&entry),
            services.retry.as_ref(),
        )
        .unwrap();
        assert_eq!(updated.status, "ok");
        assert_eq!(updated.raw_text, SIMULATED_RETRY);
        assert_eq!(updated.audio_path, entry.audio_path);
        assert!(clip.is_file(), "retry keeps the clip");

        // Deleting the row removes the clip (only possible inside the root).
        assert!(history.delete(entry.id).unwrap());
        assert!(!clip.exists(), "{}", clip.display());
        assert!(history.get(entry.id).unwrap().is_none());
    }

    #[test]
    fn seeding_is_isolated_persistent_and_resettable() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("sb");
        let root = prepare(&base, false).unwrap();
        assert!(base.join(SANDBOX_MARKER).exists());
        let history = open_history(&root).unwrap();
        assert_eq!(history.recent(100, None, 0).unwrap().len(), HISTORY.len());
        // Edits survive a relaunch.
        let dict = DictionaryStore::new(Some(root.dictionary_path.clone()));
        dict.add_word("Persisted").unwrap();
        let again = prepare(&base, false).unwrap();
        let dict = DictionaryStore::new(Some(again.dictionary_path.clone()));
        assert!(dict.words().contains(&"Persisted".to_string()));
        // Reset re-seeds.
        let reset = prepare(&base, true).unwrap();
        let dict = DictionaryStore::new(Some(reset.dictionary_path.clone()));
        assert!(!dict.words().contains(&"Persisted".to_string()));
    }

    #[test]
    fn refuses_foreign_directories() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("precious.txt"), "x").unwrap();
        assert!(prepare(dir.path(), false).is_err());
        assert!(prepare(dir.path(), true).is_err());
        assert!(dir.path().join("precious.txt").exists());
    }

    #[test]
    fn simulated_model_work_is_offline_cancellable_and_flagged() {
        use lf_ui::setup_backend::SetupModelBackend;
        use std::sync::atomic::AtomicBool;
        use std::sync::{Arc, Mutex};
        let backend = SimSetupBackend::with_step(std::time::Duration::from_millis(1));
        let model = lf_ui::setup_flow::stt_model_by_id("fw-medium").unwrap();
        let before = backend
            .speech_downloads(std::slice::from_ref(&model))
            .unwrap();
        assert!(!before["fw-medium"]);
        let ticks = Arc::new(Mutex::new(Vec::new()));
        let seen = ticks.clone();
        backend
            .download_speech(
                &model,
                Box::new(move |p| seen.lock().unwrap().push(p.done)),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert_eq!(
            *ticks.lock().unwrap().last().unwrap(),
            u64::from(model.size_mb) * 1_000_000
        );
        assert!(
            backend
                .speech_downloads(std::slice::from_ref(&model))
                .unwrap()["fw-medium"]
        );
        let cancel = Arc::new(AtomicBool::new(true));
        let small = lf_ui::setup_flow::stt_model_by_id("fw-large-v3").unwrap();
        assert!(backend
            .download_speech(&small, Box::new(|_| {}), cancel.clone())
            .is_err());
        assert!(
            !backend
                .speech_downloads(std::slice::from_ref(&small))
                .unwrap()["fw-large-v3"]
        );
        assert_eq!(
            backend.try_dictation(&model, None, None).unwrap(),
            SIMULATED_TRANSCRIPT
        );
        assert!(backend
            .try_dictation_cancellable(&model, None, None, cancel)
            .is_err());
        assert_eq!(backend.speech_format(&model), Some("Simulated"));
        let mut meter =
            crate::controllers::setup_flow::MeterInput::open(&SimMeterInput, None).unwrap();
        assert!(meter.level() > 0.0);
        meter.stop();
    }

    #[test]
    fn sandbox_download_delete_setup_and_cleanup_share_one_inventory() {
        use crate::controllers::model_storage::SpeechStorage;
        use lf_ui::setup_backend::SetupModelBackend;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let state = SimSpeech::seeded();
        let storage = SimStorage::new(state.clone());
        let backend = SimSetupBackend::with_state(state, std::time::Duration::from_millis(1));
        let models: Vec<_> = ["fw-base", "fw-small", "fw-medium"]
            .iter()
            .map(|id| lf_ui::setup_flow::stt_model_by_id(id).unwrap())
            .collect();
        let mut config = Config::defaults_for(false);
        config.set_field("stt_backend", json!("faster-whisper"));
        let whisper = |storage: &SimStorage, config: &Config| {
            storage
                .inventory(config)
                .unwrap()
                .models
                .into_iter()
                .find(|m| m.repo == lf_stt_repo::WHISPER_CPP)
        };
        let before = whisper(&storage, &config).unwrap();
        assert_eq!(before.status, "Selected");
        assert!(storage
            .delete(lf_stt_repo::WHISPER_CPP, &config)
            .unwrap_err()
            .contains("Choose another"));
        assert!(storage
            .delete(lf_stt_repo::PARAKEET_ONNX, &config)
            .unwrap_err()
            .contains("running LocalFlow"));
        // A download shows up in storage too.
        backend
            .download_speech(
                &models[2],
                Box::new(|_| {}),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        let grown = whisper(&storage, &config).unwrap();
        assert!(grown.size_bytes > before.size_bytes);
        assert!(grown.label.contains("medium"));
        // Deleting (once not selected) makes setup and the catalogue agree.
        config.set_field("stt_backend", json!("parakeet"));
        let freed = storage.delete(lf_stt_repo::WHISPER_CPP, &config).unwrap();
        assert_eq!(freed, grown.size_bytes);
        let inspected = backend.speech_downloads(&models).unwrap();
        assert!(
            inspected.values().all(|installed| !installed),
            "{inspected:?}"
        );
        assert!(whisper(&storage, &config).is_none());
        assert!(
            storage.delete(lf_stt_repo::WHISPER_CPP, &config).is_err(),
            "already gone"
        );
        // A cleanup pull lands in the simulated Ollama list.
        assert!(!backend
            .ollama("")
            .unwrap()
            .1
            .contains(&"gemma3:4b".to_string()));
        backend
            .pull_cleanup(
                "",
                "gemma3:4b",
                Box::new(|_| {}),
                Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert!(backend
            .ollama("")
            .unwrap()
            .1
            .contains(&"gemma3:4b".to_string()));
        // A cancelled pull adds nothing.
        assert!(backend
            .pull_cleanup(
                "",
                "qwen2.5:7b",
                Box::new(|_| {}),
                Arc::new(AtomicBool::new(true))
            )
            .is_err());
        assert!(!backend
            .ollama("")
            .unwrap()
            .1
            .contains(&"qwen2.5:7b".to_string()));
    }

    #[test]
    fn exercise_writes_every_sandbox_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = prepare(&dir.path().join("sb"), false).unwrap();
        let services = Services::open(root.clone()).unwrap();
        let before_config = fs::read(&root.config_path).unwrap();
        let before_history = services.history().unwrap().recent(100, None, 0).unwrap();
        let done = exercise(&services).unwrap();
        let stores: Vec<&str> = done.iter().filter_map(|l| l.split(':').next()).collect();
        assert_eq!(
            stores,
            ["settings", "models", "hot phrases", "history", "prompts"]
        );
        assert_ne!(fs::read(&root.config_path).unwrap(), before_config);
        assert_eq!(
            services.secrets.get("provider_key_groq"),
            "sandbox-exercise-key"
        );
        let phrases = fs::read_to_string(&root.hot_phrases_path).unwrap();
        assert!(phrases.contains(EXERCISE_PHRASE));
        let after_history = services.history().unwrap().recent(100, None, 0).unwrap();
        assert_eq!(after_history.len() + 1, before_history.len());
        let queue = services.remote.prompts(&services.config());
        let statuses = |status: &str| queue.list(Some(status)).unwrap().len();
        assert_eq!(
            (
                statuses("pending"),
                statuses("approved"),
                statuses("failed")
            ),
            (1, 1, 1),
            "one pending prompt approved; the failed one untouched"
        );
        let approved = done.last().unwrap();
        let id = queue.list(Some("approved")).unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(approved, &format!("prompts: approved pending prompt {id}"));
    }

    #[test]
    fn seed_queue_has_pending_and_failed() {
        let queue = seed_prompt_queue();
        let all = queue.list(None).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(queue.list(Some("failed")).unwrap().len(), 1);
    }

    fn sandbox_ask(
        root: &DataRoot,
        text: &str,
        filter: &str,
    ) -> Vec<crate::controllers::ask_agent::AgentEvent> {
        use crate::controllers::ask_agent::{answer, Question, LIMITS};
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let agent = SandboxAgent::new(
            root.clone(),
            Arc::new(seed_prompt_queue()),
            std::time::Duration::ZERO,
        );
        let mut events = Vec::new();
        let question = Question {
            text: text.into(),
            filter: filter.into(),
            context: Vec::new(),
        };
        answer(
            &agent,
            &question,
            &LIMITS,
            &AtomicBool::new(false),
            &mut |e| events.push(e),
        );
        events
    }

    #[test]
    fn sandbox_ask_runs_the_tool_loop_offline() {
        use crate::controllers::ask_agent::{AgentEvent, Via};
        use crate::controllers::mcp_client::McpStatus;
        let dir = tempfile::tempdir().unwrap();
        let root = prepare(&dir.path().join("sb"), false).unwrap();
        let events = sandbox_ask(&root, "When did the Kubernetes rollout fail?", "Any time");
        assert!(events.contains(&AgentEvent::Status(McpStatus::Simulated)));
        let names: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolStarted { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["search_dictations", "get_dictation"]);
        let Some(AgentEvent::Finished(done)) = events.last() else {
            panic!("unfinished");
        };
        assert_eq!(done.via, Via::Tools);
        assert!(done
            .turn
            .answer
            .starts_with("Sandbox answer (scripted model"));
        assert!(done.turn.answer.contains("Kubernetes"));
        let id: i64 = done.turn.citations[0].reference.parse().unwrap();
        let entry = open_history(&root).unwrap().get(id).unwrap().unwrap();
        assert!(entry.final_text.contains("Kubernetes"));

        let events = sandbox_ask(&root, "What are my next steps for the project?", "Any time");
        let Some(AgentEvent::Finished(done)) = events.last() else {
            panic!("unfinished");
        };
        assert!(
            done.turn.answer.contains("cutover checklist"),
            "{}",
            done.turn.answer
        );

        // With a time range, prompts are not searched; nothing found is said so.
        let events = sandbox_ask(&root, "zebra quantum", "Past week");
        let Some(AgentEvent::Finished(done)) = events.last() else {
            panic!("unfinished");
        };
        assert_eq!(done.turn.answer, lf_ui::backends::ask::NO_ANSWER);
        assert!(done.turn.citations.is_empty());
    }

    /// "git" is three characters: it must reach the search (not be dropped
    /// so the newest dictation answers instead) and match the git dictation.
    #[test]
    fn short_topic_words_find_their_dictation_not_the_newest() {
        use crate::controllers::ask_agent::{AgentEvent, Via};
        use crate::controllers::mcp_client::ToolTransport;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let root = prepare(&dir.path().join("sb"), false).unwrap();
        let entries = open_history(&root).unwrap().recent(100, None, 0).unwrap();
        let newest = entries.iter().find(|e| e.status == "ok").unwrap();
        let git = entries
            .iter()
            .find(|e| e.final_text.starts_with("git commit"))
            .unwrap();
        assert_ne!(newest.id, git.id, "the example must not be the newest");
        assert!(!newest.final_text.to_lowercase().contains("git"));

        // Through the production Ask agent and the sandbox model.
        let events = sandbox_ask(&root, "What did I say about git?", "Any time");
        let queries: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolStarted { name, args, .. } if name == "search_dictations" => {
                    Some(args.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(queries.len(), 1);
        assert!(queries[0].contains("query: \"git\""), "{}", queries[0]);
        let Some(AgentEvent::Finished(done)) = events.last() else {
            panic!("unfinished");
        };
        assert_eq!(done.via, Via::Tools);
        assert_eq!(done.turn.citations.len(), 1);
        let cite = &done.turn.citations[0];
        assert_eq!(cite.source, "dictation");
        assert_eq!(cite.reference, git.id.to_string());
        assert_eq!(cite.app, "Terminal");
        assert!(
            done.turn.answer.contains("git commit"),
            "{}",
            done.turn.answer
        );
        assert!(!done.turn.answer.contains(&newest.final_text));

        // The tool itself: whole-word short terms, no broadening.
        let mut tools = SandboxTools {
            root: root.clone(),
            prompts: Arc::new(seed_prompt_queue()),
            latency: std::time::Duration::ZERO,
        };
        let cancel = AtomicBool::new(false);
        let mut search = |args: Value| {
            tools
                .call("search_dictations", &args, &cancel)
                .unwrap()
                .structured
                .unwrap()
        };
        let found = search(json!({"query": "git"}));
        assert_eq!(found["count"], 1);
        assert_eq!(found["results"][0]["id"], git.id);
        // A non-empty query with nothing searchable matches nothing...
        for query in ["the", "what did I say", "!!"] {
            let none = search(json!({"query": query}));
            assert_eq!(
                (none["count"].as_i64(), none["match"].as_str()),
                (Some(0), Some("ranked")),
                "{query}"
            );
        }
        // ...while no query (or a blank one) still lists the latest.
        for args in [json!({}), json!({"query": "  "})] {
            let latest = search(args);
            assert_eq!(latest["match"], "recent");
            assert_eq!(latest["results"][0]["id"], newest.id);
        }
        // Longer terms still match word starts; short ones never inside words.
        assert_eq!(search(json!({"query": "fail"}))["count"], 1);
        assert_eq!(words("What did I say about git and CI?"), ["git", "ci"]);
        assert_eq!(term_score(&words("ci"), "Check the readiness probe"), 0);
    }

    /// The built-in "What have I dictated about the launch?" once searched
    /// for its framing words too ("have" matched "Happy birthday! Hope you
    /// have a great day."). It must search the topic only, find the one
    /// launch example and cite it, never the birthday message.
    #[test]
    fn built_in_launch_question_searches_its_topic_and_cites_the_launch_note() {
        use crate::controllers::ask_agent::{AgentEvent, Via};
        let question = lf_ui::backends::ask::EXAMPLES
            .iter()
            .find(|q| q.contains("launch"))
            .expect("built-in launch question");
        assert_eq!(question_topic(question), ["launch"]);
        // The framing word is still searchable when it is the topic.
        assert_eq!(
            question_topic("What did I dictate about dictated notes?"),
            ["dictated", "notes"]
        );
        assert_eq!(question_topic("dictated"), ["dictated"]);

        let dir = tempfile::tempdir().unwrap();
        let root = prepare(&dir.path().join("sb"), false).unwrap();
        let events = sandbox_ask(&root, question, "Any time");
        let calls: Vec<(&str, &str)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolStarted { name, args, .. } => Some((name.as_str(), args.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(calls[0].0, "search_dictations");
        assert!(calls[0].1.contains("query: \"launch\""), "{}", calls[0].1);
        // One search, then the found dictation is opened.
        assert_eq!(
            calls.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            ["search_dictations", "get_dictation"],
            "{calls:?}"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolFinished { id: 1, ok: true, summary, .. } if summary == "1 result"
        )));
        let Some(AgentEvent::Finished(done)) = events.last() else {
            panic!("unfinished");
        };
        assert_eq!(done.via, Via::Tools);
        assert!(done.turn.error.is_empty(), "{}", done.turn.error);
        assert_ne!(done.turn.answer, lf_ui::backends::ask::NO_ANSWER);
        assert!(
            done.turn.answer.contains("Launch plan"),
            "{}",
            done.turn.answer
        );
        assert!(!done.turn.answer.to_lowercase().contains("birthday"));
        assert_eq!(done.turn.citations.len(), 1, "{:?}", done.turn.citations);
        let id: i64 = done.turn.citations[0].reference.parse().unwrap();
        let entry = open_history(&root).unwrap().get(id).unwrap().unwrap();
        assert!(
            entry.final_text.starts_with("Launch plan:"),
            "{}",
            entry.final_text
        );
        assert!(!done.turn.citations[0]
            .snippet
            .to_lowercase()
            .contains("birthday"));
    }

    /// A trailing qualifier after "about" is not the topic: "What was that
    /// Kubernetes dictation about again?" searched for `again` and found
    /// nothing. It must search the words before "about" and find the example.
    #[test]
    fn trailing_qualifier_after_about_keeps_the_real_topic() {
        use crate::controllers::ask_agent::{AgentEvent, Via};
        let question = "What was that Kubernetes dictation about again?";
        assert_eq!(question_topic(question), ["kubernetes"]);
        // Explicit topics after "about" are unchanged.
        assert_eq!(
            question_topic("What have I dictated about the launch?"),
            ["launch"]
        );
        assert_eq!(
            question_topic("What did I dictate about dictated notes?"),
            ["dictated", "notes"]
        );
        // A qualifier is still searched when it is all there is.
        assert_eq!(question_topic("again"), ["again"]);
        assert_eq!(question_topic("What did I say about again?"), ["again"]);
        // Only framing before a trailing qualifier: the qualifier is the topic.
        assert_eq!(question_topic("What did I write about again?"), ["again"]);

        let dir = tempfile::tempdir().unwrap();
        let root = prepare(&dir.path().join("sb"), false).unwrap();
        let events = sandbox_ask(&root, question, "Any time");
        let calls: Vec<(&str, &str)> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolStarted { name, args, .. } => Some((name.as_str(), args.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(calls[0].0, "search_dictations");
        assert!(
            calls[0].1.contains("query: \"kubernetes\""),
            "{}",
            calls[0].1
        );
        assert_eq!(calls[1].0, "get_dictation", "{calls:?}");
        let Some(AgentEvent::Finished(done)) = events.last() else {
            panic!("unfinished");
        };
        assert_eq!(done.via, Via::Tools);
        assert!(
            done.turn.answer.contains("Kubernetes"),
            "{}",
            done.turn.answer
        );
        let id: i64 = done.turn.citations[0].reference.parse().unwrap();
        let entry = open_history(&root).unwrap().get(id).unwrap().unwrap();
        assert!(entry.final_text.contains("Kubernetes"));
    }

    /// A detailed question has more distinct terms than one query may hold
    /// (`MAX_ARG_CHARS`): the first search must still pass validation, run
    /// and answer from the fixture, not be rejected into "not found".
    #[test]
    fn detailed_questions_still_search_within_the_argument_limit() {
        use crate::controllers::ask_agent::{
            spec, validate_args, AgentEvent, Scope, Via, MAX_ARG_CHARS,
        };
        let dir = tempfile::tempdir().unwrap();
        let root = prepare(&dir.path().join("sb"), false).unwrap();
        let fillers: Vec<String> = (0..40).map(|i| format!("zylophant{i:02}")).collect();
        let question = format!(
            "When did the Kubernetes rollout fail, considering {}?",
            fillers.join(" ")
        );
        let terms = words(&question);
        assert!(
            terms.join(" ").chars().count() > MAX_ARG_CHARS,
            "the unbounded query would be rejected"
        );
        let query = bounded_query(&terms, MAX_ARG_CHARS);
        assert!(query.chars().count() <= MAX_ARG_CHARS);
        assert!(query.starts_with("kubernetes rollout fail"));
        assert!(validate_args(
            spec("search_dictations").unwrap(),
            &json!({"query": query, "limit": 5}),
            &Scope::default()
        )
        .is_ok());

        // The production loop: the first search runs and succeeds.
        let events = sandbox_ask(&root, &question, "Any time");
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::ToolFinished {
                    id: 1,
                    ok: true,
                    ..
                }
            )),
            "{events:?}"
        );
        let names: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolStarted { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["search_dictations", "get_dictation"]);
        let Some(AgentEvent::Finished(done)) = events.last() else {
            panic!("unfinished");
        };
        assert_eq!(done.via, Via::Tools);
        assert!(
            done.turn.answer.contains("Kubernetes"),
            "{}",
            done.turn.answer
        );
        let cite = &done.turn.citations[0];
        assert_eq!(cite.source, "dictation");
        let entry = open_history(&root)
            .unwrap()
            .get(cite.reference.parse().unwrap())
            .unwrap()
            .unwrap();
        assert!(entry.final_text.contains("Kubernetes rollout failed"));
    }

    #[test]
    fn bounded_queries_count_characters_and_keep_overlong_topics() {
        let s = |v: &[&str]| v.iter().map(|t| t.to_string()).collect::<Vec<_>>();
        // Unicode: characters, not bytes, and whole terms only.
        let wide: Vec<String> = (0..60).map(|i| format!("日本語{i:02}")).collect();
        let query = bounded_query(&wide, 200);
        assert!(query.len() > 200, "more bytes than characters");
        assert!(query.chars().count() <= 200);
        assert!(query.split(' ').all(|t| wide.contains(&t.to_string())));
        assert_eq!(query.split(' ').count(), 33, "5 chars, then 6 with a space");
        // An overlong first term is skipped, not the end of the query.
        let long = "a".repeat(250);
        assert_eq!(bounded_query(&s(&[&long, "git", "ci"]), 200), "git ci");
        assert_eq!(bounded_query(&s(&["git", &long, "ci"]), 200), "git ci");
        // Only overlong terms: a prefix of the first, never no query at all.
        let only = bounded_query(&s(&[&"é".repeat(250), &long]), 200);
        assert_eq!(only, "é".repeat(200));
        // Exactly at the limit, and the existing cases.
        let exact = "b".repeat(196);
        assert_eq!(
            bounded_query(&s(&[&exact, "git"]), 200),
            format!("{exact} git")
        );
        assert_eq!(bounded_query(&s(&[&exact, "gits"]), 200), exact);
        assert_eq!(bounded_query(&[], 200), "");
        assert_eq!(
            bounded_query(&words("What did I say about git?"), 200),
            "git"
        );
    }

    #[test]
    fn sandbox_pacing_never_outlasts_the_run_deadline() {
        use crate::controllers::mcp_client::{McpError, ToolTransport};
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let root = prepare(&dir.path().join("sb"), false).unwrap();
        let mut tools = SandboxTools {
            root: root.clone(),
            prompts: Arc::new(seed_prompt_queue()),
            latency: Duration::from_secs(30),
        };
        let started = Instant::now();
        assert_eq!(
            tools
                .call_until(
                    "search_dictations",
                    &json!({}),
                    &AtomicBool::new(false),
                    Instant::now() + Duration::from_millis(100),
                )
                .err(),
            Some(McpError::Deadline)
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let agent = SandboxAgent::new(root, Arc::new(seed_prompt_queue()), Duration::ZERO);
        use crate::controllers::ask_agent::AgentBackend;
        assert_eq!(
            agent
                .connect(&AtomicBool::new(false), Some(Instant::now()))
                .err(),
            Some(McpError::Deadline)
        );
        assert!(agent.connect(&AtomicBool::new(false), None).is_ok());
    }

    /// An unspaced Japanese sentence is one token: a phrase from its middle
    /// must still find it, without loosening ASCII word matching.
    #[test]
    fn japanese_phrases_match_inside_unspaced_sentences() {
        use crate::controllers::ask_agent::{AgentEvent, Via};
        use crate::controllers::mcp_client::ToolTransport;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let root = prepare(&dir.path().join("sb"), false).unwrap();
        let entries = open_history(&root).unwrap().recent(100, None, 0).unwrap();
        let meeting = entries
            .iter()
            .find(|e| e.final_text == "会議は木曜日の午後二時に移動しました。")
            .unwrap();

        let mut tools = SandboxTools {
            root: root.clone(),
            prompts: Arc::new(seed_prompt_queue()),
            latency: std::time::Duration::ZERO,
        };
        let cancel = AtomicBool::new(false);
        let mut search = |query: &str| {
            tools
                .call("search_dictations", &json!({ "query": query }), &cancel)
                .unwrap()
                .structured
                .unwrap()
        };
        for phrase in [
            "午後二時",
            "木曜日",
            "移動",
            "会議は木曜日の午後二時に移動しました",
        ] {
            let found = search(phrase);
            assert_eq!(found["count"], 1, "{phrase}");
            assert_eq!(found["results"][0]["id"], meeting.id, "{phrase}");
        }
        assert_eq!(search("午前九時")["count"], 0);

        // Through the production Ask agent.
        let events = sandbox_ask(&root, "午後二時", "Any time");
        let Some(AgentEvent::Finished(done)) = events.last() else {
            panic!("unfinished");
        };
        assert_eq!(done.via, Via::Tools);
        assert_eq!(done.turn.citations.len(), 1);
        assert_eq!(done.turn.citations[0].reference, meeting.id.to_string());
        assert!(
            done.turn.answer.contains("午後二時"),
            "{}",
            done.turn.answer
        );

        // ASCII semantics unchanged.
        let t = |s: &str| vec![s.to_string()];
        assert_eq!(
            term_score(&t("午後二時"), "会議は木曜日の午後二時に移動しました。"),
            1
        );
        assert_eq!(term_score(&t("git"), "digital git"), 1);
        assert_eq!(term_score(&t("git"), "digital"), 0);
        assert_eq!(term_score(&t("fail"), "failed"), 1);
        assert_eq!(term_score(&t("fail"), "unfailing"), 0);
    }

    #[test]
    fn sandbox_tools_follow_the_server_shapes_and_bounds() {
        use crate::controllers::mcp_client::ToolTransport;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let root = prepare(&dir.path().join("sb"), false).unwrap();
        let mut tools = SandboxTools {
            root: root.clone(),
            prompts: Arc::new(seed_prompt_queue()),
            latency: std::time::Duration::ZERO,
        };
        let cancel = AtomicBool::new(false);
        let recent = tools
            .call("search_dictations", &json!({"limit": 2}), &cancel)
            .unwrap();
        let data = recent.structured.unwrap();
        assert_eq!(
            (data["count"].as_i64(), data["match"].as_str()),
            (Some(2), Some("recent"))
        );
        // Every example is from the last few hours: none before today's start minus a day.
        let old = tools
            .call(
                "search_dictations",
                &json!({"until": "2000-01-01"}),
                &cancel,
            )
            .unwrap();
        assert_eq!(old.structured.unwrap()["count"], 0);
        let missing = tools
            .call("get_dictation", &json!({"id": 999_999}), &cancel)
            .unwrap();
        assert!(missing.is_error);
        let failed = tools
            .call("search_prompts", &json!({"status": "failed"}), &cancel)
            .unwrap();
        assert_eq!(failed.structured.unwrap()["count"], 1);
        assert!(tools.call("exec", &json!({}), &cancel).is_err());
        assert!(tools
            .call("search_dictations", &json!({}), &AtomicBool::new(true))
            .is_err());
    }
}
