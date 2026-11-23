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

use crate::data::{open_history, DataRoot, SANDBOX_MARKER};

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
            entry.audio_path = Some(history.write_clip(&tone, 16_000)?.to_string_lossy().into());
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
/// the model succeeds only while the simulated inventory lists the selected
/// backend's repo, so deleting it on Models & keys shows "not downloaded".
/// No cleanup provider (a real one could make an HTTP call) and no keys.
pub struct SimRetry {
    storage: std::sync::Arc<dyn crate::controllers::model_storage::SpeechStorage>,
}

impl SimRetry {
    pub fn new(
        storage: std::sync::Arc<dyn crate::controllers::model_storage::SpeechStorage>,
    ) -> Self {
        SimRetry { storage }
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
        let backend = config.field_str("stt_backend").unwrap_or("");
        let repo = if backend.starts_with("parakeet") {
            lf_stt_repo::PARAKEET_ONNX
        } else {
            lf_stt_repo::WHISPER_CPP
        };
        if !self
            .storage
            .inventory(config)?
            .models
            .iter()
            .any(|m| m.repo == repo)
        {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_retry_is_a_labelled_simulation_over_sandbox_data_only() {
        use crate::controllers::model_storage::SpeechStorage;
        use crate::controllers::retry::{retry_entry, RetryBackend, RetryKey};
        let dir = tempfile::tempdir().unwrap();
        let root = prepare(&dir.path().join("sb"), false).unwrap();
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
        let shown = crate::controllers::retry::view(
            &entry,
            services.retry.as_ref(),
            &services.config(),
            &services.retry_jobs,
        );
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
        let storage = std::sync::Arc::new(SimStorage::new(sim));
        let mut other = Config::defaults_for(false);
        other.set_field("stt_backend", json!("parakeet"));
        storage.delete(lf_stt_repo::WHISPER_CPP, &other).unwrap();
        let retry = SimRetry::new(storage);
        let error = retry.open(&services.config()).err().unwrap();
        assert!(error.contains("isn't downloaded"), "{error}");
        for path in services.root.all_paths() {
            assert!(path.starts_with(dir.path()));
        }
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
    fn seed_queue_has_pending_and_failed() {
        let queue = seed_prompt_queue();
        let all = queue.list(None).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(queue.list(Some("failed")).unwrap().len(), 1);
    }
}
