//! History → Retry: transcribe a saved dictation clip again and update that
//! same row.
//!
//! The work is the shared `lf_runtime::pipeline::process_clip` (silence gate,
//! STT with the dictionary prompt, hot phrases, replacements, formatting,
//! fail-soft LLM cleanup, app style) with the current config, vocabulary read
//! from this window's [`DataRoot`] and the row's own app context. Nothing is
//! pasted, copied, written to `last.txt` or pruned, and the config is never
//! written. The commit is `HistoryStore::update_if_unchanged`: one atomic
//! conditional UPDATE, so a deleted, cleared-and-reused or concurrently
//! edited row is never overwritten and no row is ever created.
//!
//! One job per process ([`RetryJobs`], shared through `Services`) runs on its
//! own OS thread, registered as a pending write so a close waits for it
//! (bounded) without depending on the UI executor. Speech recognition can't
//! be interrupted, so nothing claims to cancel it.
//!
//! When the configured native model isn't downloaded (or the native engine
//! has no such size, e.g. `distil-large-v3`), a real (never sandbox) window transcribes with the Python app's model instead
//! ([`PythonRetry`]) and commits its result the same way.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use lf_core::config::Config;
use lf_core::dictionary::DictionaryStore;
use lf_core::history::{EntryUpdate, HistoryEntry};
use lf_core::hotphrases::HotPhraseStore;
use lf_core::symbols::discover_commands;
use lf_providers::LlmProvider;
use lf_runtime::context::AppInfo;
use lf_runtime::engine::SpeechEngine;
use lf_runtime::pipeline::{process_clip, Models, Vocabulary};

use crate::controllers::retry_python::{PythonError, PythonRetry, Transcript};
use crate::data::{open_history, DataRoot};

/// The only clip rate the native engines accept.
pub const CLIP_RATE: u32 = 16_000;

/// How `lf_stt::runtime::hub_speech_cached` says the configured model isn't
/// installed.
pub const NOT_DOWNLOADED: &str = "isn't downloaded";

/// How the native engine says it has no such model size (a size or repo
/// only faster-whisper has). `lf_stt::engines::UNSUPPORTED_WHISPER_SIZE`,
/// copied because default builds don't link lf-stt (a test pins it).
pub const NO_NATIVE_SIZE: &str = "unsupported whisper.cpp model size";

/// What a missing native model's error ends with.
pub const MODELS_HINT: &str = "Download it in Models & keys, then try again.";

/// Persistent supporting text when results are simulated (sandbox).
pub const SIMULATED_NOTE: &str = "Sandbox: simulated result. No speech model ran.";

/// Where a retry gets its models. Implementations: the native engine
/// (`runtime` builds), an honest "unavailable" (default builds) and the
/// sandbox simulation.
pub trait RetryBackend: Send + Sync {
    /// Why Retry can't run in this build or launch at all. No I/O: safe to
    /// call while rendering.
    fn unavailable(&self) -> Option<String>;
    /// The configured, already installed speech model. Never downloads and
    /// never selects another model. Blocking; worker thread only.
    fn open(&self, config: &Config) -> Result<Arc<dyn SpeechEngine>, String>;
    /// Cleanup provider, exactly as live dictation builds it (`None`: no
    /// cleanup). Worker thread only.
    fn llm(&self, config: &Config) -> Option<Box<dyn LlmProvider>>;
    /// Results come from a deterministic simulation, not a model.
    fn simulated(&self) -> bool {
        false
    }
    /// The Python app's model, used when `open` reports the configured
    /// native model isn't downloaded (real windows only).
    fn python(&self) -> Option<&PythonRetry> {
        None
    }
}

/// Default builds: no speech engine is compiled in.
pub struct UnavailableRetry;

impl RetryBackend for UnavailableRetry {
    fn unavailable(&self) -> Option<String> {
        Some(format!(
            "{} Rebuild with `--features runtime` ({}) or use the Python Hub.",
            lf_ui::hub_model::HISTORY_RETRY_UNAVAILABLE,
            crate::controllers::capabilities::REBUILD
        ))
    }
    fn open(&self, _: &Config) -> Result<Arc<dyn SpeechEngine>, String> {
        Err(self.unavailable().unwrap_or_default())
    }
    fn llm(&self, _: &Config) -> Option<Box<dyn LlmProvider>> {
        None
    }
}

/// `runtime` builds: the installed configured model through
/// `lf_stt::runtime::hub_speech_cached` (the speech helper on macOS, in
/// process elsewhere), with scoped model leases while loaded.
#[cfg(feature = "runtime")]
pub struct NativeRetry {
    pub secrets: Arc<dyn lf_providers::SecretStore + Send + Sync>,
    pub python: PythonRetry,
}

#[cfg(feature = "runtime")]
impl RetryBackend for NativeRetry {
    fn unavailable(&self) -> Option<String> {
        None
    }
    fn open(&self, config: &Config) -> Result<Arc<dyn SpeechEngine>, String> {
        let speech = lf_stt::runtime::hub_speech_cached(config).map_err(|error| {
            if error.contains(NOT_DOWNLOADED) {
                format!("{error}. {MODELS_HINT}")
            } else {
                error
            }
        })?;
        Ok(Arc::new(ReleaseAfterUnload(speech)))
    }
    fn llm(&self, config: &Config) -> Option<Box<dyn LlmProvider>> {
        lf_providers::factory::build_llm(
            config,
            &lf_providers::factory::BuildOptions::default(),
            self.secrets.as_ref(),
        )
        .ok()
    }
    fn python(&self) -> Option<&PythonRetry> {
        Some(&self.python)
    }
}

/// Returns allocator pages once the temporary model is unloaded.
#[cfg(feature = "runtime")]
struct ReleaseAfterUnload(Arc<dyn SpeechEngine>);

#[cfg(feature = "runtime")]
impl SpeechEngine for ReleaseAfterUnload {
    fn loaded(&self) -> bool {
        self.0.loaded()
    }
    fn load(&self) -> Result<(), String> {
        self.0.load()
    }
    fn transcribe(&self, pcm: &[f32], prompt: Option<&str>) -> Result<String, String> {
        self.0.transcribe(pcm, prompt)
    }
    fn unload(&self) {
        self.0.unload();
        lf_stt::release_idle_memory();
    }
}

/// Why a retry didn't update the row. The row is untouched in every case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RetryError {
    Gone,
    NotRetryable,
    Clip(String),
    Unavailable(String),
    Failed(String),
    Empty,
    Changed,
    Panicked,
    Start(String),
}

impl RetryError {
    /// One line for the error banner (after "Retry failed.").
    pub fn message(&self) -> String {
        match self {
            RetryError::Gone => "This dictation is no longer in your history.".into(),
            RetryError::NotRetryable => "Only dictations with saved audio can be retried.".into(),
            RetryError::Clip(why) => format!("The saved audio can't be used: {why}."),
            RetryError::Unavailable(why) => why.clone(),
            RetryError::Failed(why) => format!("Transcription failed: {why}"),
            RetryError::Empty => {
                "No speech was recognised in the saved audio, so nothing was changed.".into()
            }
            RetryError::Changed => {
                "This dictation changed while it was being retried, so the result wasn't saved."
                    .into()
            }
            RetryError::Panicked => "Retry stopped unexpectedly. Nothing was changed.".into(),
            RetryError::Start(why) => format!("Retry couldn't start: {why}"),
        }
    }
}

/// The config one retry runs with: `config` with `sample_rate` set to the
/// saved clip's own (already validated) rate. The microphone `sample_rate`
/// setting describes live capture, not saved clips (always stored at
/// [`CLIP_RATE`]), so it must not block or skew a retry. A local copy only:
/// every other field is kept and nothing is written back.
fn clip_config(config: &Config, clip_rate: u32) -> Config {
    let mut config = config.clone();
    config.set_field("sample_rate", serde_json::json!(clip_rate));
    config
}

/// A row's identity for a job. Ids are reused after a clear, so the
/// timestamp is part of it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RetryKey {
    pub id: i64,
    pub ts: f64,
}

impl RetryKey {
    pub fn of(entry: &HistoryEntry) -> Self {
        RetryKey {
            id: entry.id,
            ts: entry.ts,
        }
    }
    pub fn matches(&self, entry: &HistoryEntry) -> bool {
        self.id == entry.id && self.ts == entry.ts
    }
}

/// The dictionary, snippets and commands at this root's explicit paths
/// (the same fields as `lf_runtime::run::default_vocabulary`, never its
/// default-path stores).
pub fn vocabulary(root: &DataRoot, config: &Config) -> Vocabulary {
    let dictionary = DictionaryStore::new(Some(root.dictionary_path.clone()));
    Vocabulary {
        words: dictionary.words(),
        replacements: dictionary.replacements(),
        hot_phrases: if config.field_bool("hot_phrases").unwrap_or(true) {
            HotPhraseStore::new(root.hot_phrases_path.clone())
                .enabled()
                .unwrap_or_default()
        } else {
            Vec::new()
        },
        commands: discover_commands(
            &config.field_str_list("command_names").unwrap_or_default(),
            &config.field_str_list("skills_dirs").unwrap_or_default(),
        ),
    }
}

/// Unloads (and so releases the model's lease) on every exit, panics too.
struct Opened(Arc<dyn SpeechEngine>);

impl Drop for Opened {
    fn drop(&mut self) {
        self.0.unload();
    }
}

/// Retry the row `key` names. Blocking (model load and inference): run it
/// on a worker. Returns the updated row.
pub fn retry_entry(
    root: &DataRoot,
    config: &Config,
    key: RetryKey,
    backend: &dyn RetryBackend,
) -> Result<HistoryEntry, RetryError> {
    let store = open_history(root).map_err(|e| RetryError::Failed(format!("{e:#}")))?;
    let entry = store
        .get(key.id)
        .map_err(|e| RetryError::Failed(format!("{e:#}")))?
        .filter(|entry| key.matches(entry))
        .ok_or(RetryError::Gone)?;
    let path = entry
        .audio_path
        .clone()
        .filter(|p| entry.kind == "dictate" && !p.is_empty())
        .ok_or(RetryError::NotRetryable)?;
    if let Some(reason) = backend.unavailable() {
        return Err(RetryError::Unavailable(reason));
    }
    // Validated loader: inside the clips directory, mono 16-bit WAV.
    let (samples, rate) = store
        .load_clip(Path::new(&path))
        .map_err(|e| RetryError::Clip(format!("{e:#}")))?;
    if rate != CLIP_RATE {
        return Err(RetryError::Clip(format!(
            "it was saved at {rate} Hz and retry needs {CLIP_RATE} Hz"
        )));
    }
    if samples.is_empty() {
        return Err(RetryError::Clip("it contains no audio".into()));
    }
    let config = &clip_config(config, rate);
    let vocab = vocabulary(root, config);
    let app_info = (entry.app_name.is_some() || entry.bundle_id.is_some()).then(|| AppInfo {
        name: entry.app_name.clone().unwrap_or_default(),
        bundle_id: entry.bundle_id.clone().unwrap_or_default(),
    });
    let (raw, final_text) = match backend.open(config) {
        Ok(speech) => {
            let speech = Opened(speech);
            let llm = backend.llm(config);
            let models = Models {
                speech: speech.0.as_ref(),
                llm: llm.as_deref(),
            };
            let processed = process_clip(&samples, config, &models, &vocab, app_info.as_ref());
            drop(speech);
            processed.map_err(RetryError::Failed)?
        }
        Err(why) => {
            let no_size = why.contains(NO_NATIVE_SIZE);
            match backend
                .python()
                .filter(|_| (no_size || why.contains(NOT_DOWNLOADED)) && !root.is_sandbox())
            {
                Some(python) => {
                    let python_only = no_size.then(|| config.field_str("model_size").unwrap_or(""));
                    python_text(python.transcribe(root, entry.id), &entry, &why, python_only)?
                }
                None => return Err(RetryError::Unavailable(why)),
            }
        }
    };
    if final_text.trim().is_empty() {
        return Err(RetryError::Empty);
    }
    let changes = EntryUpdate {
        status: Some("ok".into()),
        raw_text: Some(raw),
        final_text: Some(final_text),
        ..Default::default()
    };
    store
        .update_if_unchanged(&entry, &changes)
        .map_err(|e| RetryError::Failed(format!("{e:#}")))?
        .ok_or(RetryError::Changed)
}

/// The Python app's result for `entry`, or why there is none. `why` is the
/// native error: the model "isn't downloaded", or (`python_only`: the
/// configured size) the native engine has no such size at all.
fn python_text(
    result: Result<Transcript, PythonError>,
    entry: &HistoryEntry,
    why: &str,
    python_only: Option<&str>,
) -> Result<(String, String), RetryError> {
    let both_ways = |reason: &str| {
        RetryError::Unavailable(match python_only {
            Some(size) => {
                let size = if size.is_empty() {
                    "configured".into()
                } else {
                    format!("`{size}`")
                };
                format!(
                    "The {size} speech model only runs in the Python LocalFlow app, and {reason}. Download the Python app's model with `lf models download`, or pick another model in Models & keys, then try again."
                )
            }
            None => {
                let missing = why.strip_suffix(MODELS_HINT).unwrap_or(why).trim_end();
                let missing = missing.strip_suffix('.').unwrap_or(missing);
                format!(
                    "{missing}, and {reason}. Download it in Models & keys, or download the Python app's model with `lf models download`, then try again."
                )
            }
        })
    };
    match result {
        Ok(t) if (RetryKey { id: t.id, ts: t.ts }).matches(entry) => Ok((t.raw_text, t.final_text)),
        Ok(_) => Err(RetryError::Failed(
            "the Python app read a different history, so nothing was changed.".into(),
        )),
        Err(PythonError::Missing) => Err(both_ways("the Python LocalFlow app wasn't found")),
        Err(PythonError::Unsupported) => Err(both_ways(
            "the `lf` found isn't a Python LocalFlow app that can retry",
        )),
        Err(PythonError::NoModel) => Err(both_ways(if python_only.is_some() {
            "it isn't downloaded there"
        } else {
            "the Python app's model isn't downloaded either"
        })),
        Err(PythonError::Failed(why)) => Err(RetryError::Failed(why)),
    }
}

/// Shown when another row's retry is running.
pub const OTHER_RUNNING: &str =
    "Another dictation is being retried. You can retry this one when it finishes.";

/// What the detail pane shows for Retry on one row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryView {
    /// This row is being retried (label "Retrying…", stays focusable).
    pub mine: bool,
    /// Why the button is disabled.
    pub blocked: Option<String>,
    /// The last retry of this row failed (persistent banner with Try again).
    pub error: Option<String>,
    /// Supporting line under the actions.
    pub note: String,
}

/// Retry's state for `entry`. No I/O: safe while rendering.
pub fn view(entry: &HistoryEntry, backend: &dyn RetryBackend, jobs: &RetryJobs) -> RetryView {
    let running = jobs.running();
    let mine = running.is_some_and(|key| key.matches(entry));
    let unavailable = backend.unavailable();
    let blocked = unavailable
        .clone()
        .or_else(|| (running.is_some() && !mine).then(|| OTHER_RUNNING.to_string()));
    let error = if mine || blocked.is_some() {
        None
    } else {
        jobs.last_for(entry)
            .and_then(|f| f.outcome.err())
            .map(|e| e.message())
    };
    let note = if let Some(reason) = &unavailable {
        reason.clone()
    } else if mine {
        "Transcribing the saved audio again with your current settings. This can't be cancelled; you can keep using LocalFlow.".into()
    } else if running.is_some() {
        OTHER_RUNNING.into()
    } else {
        "Retry transcribes the saved audio again with your current settings and replaces this text. Nothing is pasted or copied.".into()
    };
    let note = if backend.simulated() {
        format!("{SIMULATED_NOTE} {note}")
    } else {
        note
    };
    RetryView {
        mine,
        blocked,
        error,
        note,
    }
}

/// A finished job, kept until the next one finishes.
#[derive(Clone, Debug, PartialEq)]
pub struct Finished {
    pub key: RetryKey,
    /// Increases with every finished job.
    pub seq: u64,
    pub outcome: Result<HistoryEntry, RetryError>,
}

/// How many finished jobs are kept for an observer that hasn't seen them.
/// Each job needs its own click and only one runs at a time, so far fewer
/// than this can finish between two polls; an observer that still falls
/// behind is told to reload instead of missing a row.
pub const KEPT_RESULTS: usize = 16;

#[derive(Default)]
struct State {
    running: Option<RetryKey>,
    /// Oldest first, at most [`KEPT_RESULTS`].
    finished: std::collections::VecDeque<Finished>,
    seq: u64,
}

/// What [`RetryJobs::observe`] found since an observer's last look.
#[derive(Debug, Default, PartialEq)]
pub struct Observed {
    /// Rows a finished retry updated, oldest first, each reported once.
    pub updated: Vec<HistoryEntry>,
    /// Results were dropped before being observed: reload the list.
    pub missed: bool,
    /// A job was still running in this same snapshot: observe again later.
    pub running: bool,
}

/// The process's one retry slot. Cheap to clone; every clone shares it.
#[derive(Clone, Default)]
pub struct RetryJobs(Arc<Mutex<State>>);

/// Runs a job body on a new thread (injectable so tests can fail it).
pub type Spawn = dyn FnOnce(Box<dyn FnOnce() + Send>) -> std::io::Result<()>;

impl RetryJobs {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The row being retried, if any.
    pub fn running(&self) -> Option<RetryKey> {
        self.state().running
    }

    pub fn last(&self) -> Option<Finished> {
        self.state().finished.back().cloned()
    }

    /// The sequence number of the latest finished job (0: none yet).
    /// Monotonic; it moves only after that job's commit.
    pub fn seq(&self) -> u64 {
        self.state().seq
    }

    /// Every job that finished after `*seen`, under one lock (so nothing
    /// finishing concurrently is skipped or reported twice); advances `*seen`.
    pub fn observe(&self, seen: &mut u64) -> Observed {
        let state = self.state();
        let oldest = state.finished.front().map_or(state.seq + 1, |f| f.seq);
        let observed = Observed {
            missed: *seen < state.seq && oldest > *seen + 1,
            running: state.running.is_some(),
            updated: state
                .finished
                .iter()
                .filter(|f| f.seq > *seen)
                .filter_map(|f| f.outcome.clone().ok())
                .collect(),
        };
        *seen = state.seq;
        observed
    }

    /// The last finished job's outcome for `entry`, if that's the row it was for.
    pub fn last_for(&self, entry: &HistoryEntry) -> Option<Finished> {
        self.last().filter(|f| f.key.matches(entry))
    }

    /// Start retrying `key` on a new thread. `false` (and nothing started)
    /// while any job runs: repeat clicks and other rows never queue a load.
    pub fn start(
        &self,
        root: DataRoot,
        config: Config,
        key: RetryKey,
        backend: Arc<dyn RetryBackend>,
    ) -> bool {
        self.start_with(
            key,
            move || retry_entry(&root, &config, key, backend.as_ref()),
            Box::new(|body| {
                std::thread::Builder::new()
                    .name("history-retry".into())
                    .spawn(body)
                    .map(|_| ())
            }),
        )
    }

    fn start_with(
        &self,
        key: RetryKey,
        work: impl FnOnce() -> Result<HistoryEntry, RetryError> + Send + 'static,
        spawn: Box<Spawn>,
    ) -> bool {
        {
            let mut state = self.state();
            if state.running.is_some() {
                return false;
            }
            state.running = Some(key);
        }
        // Registered before the thread exists; dropped (released) with the
        // body if the thread can't be created, else after the result is in.
        let write = lf_ui::shutdown::register_pending_write();
        let jobs = self.clone();
        let body = Box::new(move || {
            let _write = write;
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or(Err(RetryError::Panicked));
            jobs.finish(key, outcome);
        });
        if let Err(error) = spawn(body) {
            self.finish(key, Err(RetryError::Start(error.to_string())));
        }
        true
    }

    fn finish(&self, key: RetryKey, outcome: Result<HistoryEntry, RetryError>) {
        let mut state = self.state();
        state.seq += 1;
        let seq = state.seq;
        if state.finished.len() == KEPT_RESULTS {
            state.finished.pop_front();
        }
        state.finished.push_back(Finished { key, seq, outcome });
        state.running = None;
    }

    /// Wait (bounded) for a running job. Test helper.
    #[cfg(test)]
    fn wait_idle(&self, wait: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + wait;
        while self.running().is_some() {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_core::history::{HistoryStore, NewEntry};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(10);

    /// What the fake speech engine does when asked to transcribe.
    #[derive(Clone)]
    enum Says {
        Text(&'static str),
        Fails,
        Panics,
    }

    #[derive(Default)]
    struct Counts {
        opens: AtomicUsize,
        loads: AtomicUsize,
        transcribes: AtomicUsize,
        unloads: AtomicUsize,
        llms: AtomicUsize,
        prompts: Mutex<Vec<Option<String>>>,
        /// `sample_rate` of every config handed to `open` and `llm`.
        rates: Mutex<Vec<Option<i64>>>,
    }

    impl Counts {
        fn saw(&self, config: &Config) {
            self.rates
                .lock()
                .unwrap()
                .push(config.field_i64("sample_rate"));
        }
    }

    struct FakeSpeech {
        says: Says,
        counts: Arc<Counts>,
        loaded: AtomicBool,
        /// Blocks inference until the test sends (a gated, mid-flight job).
        gate: Option<Mutex<mpsc::Receiver<()>>>,
        started: Option<mpsc::Sender<()>>,
    }

    impl SpeechEngine for FakeSpeech {
        fn loaded(&self) -> bool {
            self.loaded.load(Ordering::SeqCst)
        }
        fn load(&self) -> Result<(), String> {
            self.counts.loads.fetch_add(1, Ordering::SeqCst);
            self.loaded.store(true, Ordering::SeqCst);
            Ok(())
        }
        fn transcribe(&self, _: &[f32], prompt: Option<&str>) -> Result<String, String> {
            self.counts.transcribes.fetch_add(1, Ordering::SeqCst);
            self.counts
                .prompts
                .lock()
                .unwrap()
                .push(prompt.map(str::to_owned));
            if let Some(started) = &self.started {
                let _ = started.send(());
            }
            if let Some(gate) = &self.gate {
                let _ = gate.lock().unwrap().recv_timeout(WAIT);
            }
            match &self.says {
                Says::Text(text) => Ok((*text).into()),
                Says::Fails => Err("fake inference failed".into()),
                Says::Panics => panic!("fake inference panicked"),
            }
        }
        fn unload(&self) {
            self.counts.unloads.fetch_add(1, Ordering::SeqCst);
            self.loaded.store(false, Ordering::SeqCst);
        }
    }

    struct FailingLlm;
    impl LlmProvider for FailingLlm {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn offline(&self) -> bool {
            true
        }
        fn complete(
            &self,
            _: &str,
            _: &str,
            _: Option<i64>,
        ) -> Result<String, lf_providers::base::ProviderError> {
            Err(lf_providers::base::ProviderError(
                "fake provider down".into(),
            ))
        }
        fn list_models(
            &self,
        ) -> Result<Vec<lf_providers::base::ModelInfo>, lf_providers::base::ProviderError> {
            Ok(Vec::new())
        }
        fn healthcheck(&self) -> lf_providers::base::Health {
            lf_providers::base::Health::ok()
        }
    }

    struct Fake {
        says: Says,
        counts: Arc<Counts>,
        open_error: Option<&'static str>,
        failing_llm: bool,
        gate: Mutex<Option<mpsc::Receiver<()>>>,
        started: Option<mpsc::Sender<()>>,
        python: Option<PythonRetry>,
    }

    impl Fake {
        fn new(says: Says) -> Self {
            Fake {
                says,
                counts: Arc::default(),
                open_error: None,
                failing_llm: false,
                gate: Mutex::new(None),
                started: None,
                python: None,
            }
        }
        /// Inference waits for `go`; `started` fires once it's running.
        fn gated(says: Says) -> (Self, mpsc::Sender<()>, mpsc::Receiver<()>) {
            let (go, gate) = mpsc::channel();
            let (started, running) = mpsc::channel();
            let mut fake = Fake::new(says);
            fake.gate = Mutex::new(Some(gate));
            fake.started = Some(started);
            (fake, go, running)
        }
        fn opens(&self) -> usize {
            self.counts.opens.load(Ordering::SeqCst)
        }
    }

    impl RetryBackend for Fake {
        fn unavailable(&self) -> Option<String> {
            None
        }
        fn open(&self, config: &Config) -> Result<Arc<dyn SpeechEngine>, String> {
            self.counts.saw(config);
            self.counts.opens.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = self.open_error {
                return Err(error.into());
            }
            Ok(Arc::new(FakeSpeech {
                says: self.says.clone(),
                counts: self.counts.clone(),
                loaded: AtomicBool::new(false),
                gate: self.gate.lock().unwrap().take().map(Mutex::new),
                started: self.started.clone(),
            }))
        }
        fn llm(&self, config: &Config) -> Option<Box<dyn LlmProvider>> {
            self.counts.saw(config);
            self.counts.llms.fetch_add(1, Ordering::SeqCst);
            self.failing_llm
                .then(|| Box::new(FailingLlm) as Box<dyn LlmProvider>)
        }
        fn python(&self) -> Option<&PythonRetry> {
            self.python.as_ref()
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        root: DataRoot,
        store: HistoryStore,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = DataRoot::sandbox(dir.path());
        let store = open_history(&root).unwrap();
        Fixture {
            _dir: dir,
            root,
            store,
        }
    }

    fn tone() -> Vec<f32> {
        (0..16_000).map(|i| (i as f32 * 0.05).sin() * 0.2).collect()
    }

    impl Fixture {
        /// A failed dictation from Mail with a real saved clip.
        fn failed_row(&self, audio: Option<String>) -> HistoryEntry {
            let mut new = NewEntry::new("dictate");
            new.status = "failed".into();
            new.app_name = Some("Mail".into());
            new.bundle_id = Some("com.apple.mail".into());
            new.category = Some("email".into());
            new.duration_s = 1.0;
            new.audio_path = audio;
            let id = self.store.add(&new).unwrap().unwrap();
            self.store.get(id).unwrap().unwrap()
        }
        fn clip(&self) -> String {
            self.store
                .write_clip(&tone(), 16_000)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        }
        fn retryable(&self) -> HistoryEntry {
            let clip = self.clip();
            self.failed_row(Some(clip))
        }
        fn rows(&self) -> Vec<HistoryEntry> {
            self.store.recent(1000, None, 0).unwrap()
        }
        fn retry(
            &self,
            config: &Config,
            entry: &HistoryEntry,
            fake: &Fake,
        ) -> Result<HistoryEntry, RetryError> {
            retry_entry(&self.root, config, RetryKey::of(entry), fake)
        }
    }

    fn config() -> Config {
        let mut config = Config::defaults_for(false);
        config.set_field("cleanup_enabled", serde_json::json!(false));
        config
    }

    #[test]
    fn rows_without_a_safe_saved_dictate_clip_never_open_a_model() {
        let f = fixture();
        let fake = Fake::new(Says::Text("hello"));
        let none = f.failed_row(None);
        let empty = f.failed_row(Some(String::new()));
        let missing = f.failed_row(Some(
            f.root
                .clips_dir
                .join("missing.wav")
                .to_string_lossy()
                .into(),
        ));
        let outside_dir = tempfile::tempdir().unwrap();
        let outside_file = outside_dir.path().join("x.wav");
        std::fs::copy(f.clip(), &outside_file).unwrap();
        let outside = f.failed_row(Some(outside_file.to_string_lossy().into()));
        let mut command = NewEntry::new("command");
        command.audio_path = Some(f.clip());
        let command_id = f.store.add(&command).unwrap().unwrap();
        let command = f.store.get(command_id).unwrap().unwrap();
        for (entry, expected) in [
            (&none, RetryError::NotRetryable),
            (&empty, RetryError::NotRetryable),
            (&command, RetryError::NotRetryable),
        ] {
            assert_eq!(f.retry(&config(), entry, &fake).unwrap_err(), expected);
        }
        for entry in [&missing, &outside] {
            assert!(matches!(
                f.retry(&config(), entry, &fake),
                Err(RetryError::Clip(_))
            ));
        }
        #[cfg(unix)]
        {
            let link = f.root.clips_dir.join("escape.wav");
            std::os::unix::fs::symlink(&outside_file, &link).unwrap();
            let linked = f.failed_row(Some(link.to_string_lossy().into()));
            assert!(matches!(
                f.retry(&config(), &linked, &fake),
                Err(RetryError::Clip(_))
            ));
        }
        // A key whose timestamp differs (a reused id) is a different row.
        let mut reused = f.retryable();
        reused.ts += 1.0;
        assert_eq!(
            f.retry(&config(), &reused, &fake).unwrap_err(),
            RetryError::Gone
        );
        assert_eq!(fake.opens(), 0);
    }

    #[test]
    fn invalid_audio_and_unsupported_rates_fail_before_loading_speech() {
        let f = fixture();
        let fake = Fake::new(Say::text());
        let write = |name: &str, bytes: &[u8]| {
            std::fs::create_dir_all(&f.root.clips_dir).unwrap();
            let path = f.root.clips_dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            f.failed_row(Some(path.to_string_lossy().into()))
        };
        let garbage = write("garbage.wav", b"not a wav at all");
        let header = |rate: u32, channels: u16, bits: u16, data: &[u8]| {
            let mut wav = Vec::new();
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16u32.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&channels.to_le_bytes());
            wav.extend_from_slice(&rate.to_le_bytes());
            let align = channels * bits / 8;
            wav.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
            wav.extend_from_slice(&align.to_le_bytes());
            wav.extend_from_slice(&bits.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
            wav.extend_from_slice(data);
            wav
        };
        let samples = [0x10u8, 0x20, 0x30, 0x40];
        let rate_44k = write("44k.wav", &header(44_100, 1, 16, &samples));
        let stereo = write("stereo.wav", &header(16_000, 2, 16, &samples));
        let eight_bit = write("8bit.wav", &header(16_000, 1, 8, &samples));
        let empty = write("empty.wav", &header(16_000, 1, 16, &[]));
        for entry in [&garbage, &rate_44k, &stereo, &eight_bit, &empty] {
            let error = f.retry(&config(), entry, &fake).unwrap_err();
            assert!(matches!(error, RetryError::Clip(_)), "{error:?}");
        }
        assert!(f
            .retry(&config(), &rate_44k, &fake)
            .unwrap_err()
            .message()
            .contains("44100 Hz"));
        // The clip's own rate decides, whatever the microphone is set to.
        let mut matching = config();
        matching.set_field("sample_rate", serde_json::json!(44_100));
        assert!(matches!(
            f.retry(&matching, &rate_44k, &fake),
            Err(RetryError::Clip(_))
        ));
        assert_eq!(fake.opens(), 0);
        assert_eq!(fake.counts.llms.load(Ordering::SeqCst), 0);
        for entry in f.rows() {
            assert_ne!(entry.status, "ok", "nothing was updated");
        }
    }

    /// Short-hand for the common fake transcript.
    struct Say;
    impl Say {
        fn text() -> Says {
            Says::Text("teh quarterly review moved to thursday.")
        }
    }

    #[test]
    fn success_runs_the_shared_pipeline_and_updates_only_the_original_row() {
        let f = fixture();
        let dictionary = DictionaryStore::new(Some(f.root.dictionary_path.clone()));
        dictionary.add_word("LocalFlow").unwrap();
        dictionary.add_replacement("teh", "the").unwrap();
        HotPhraseStore::new(f.root.hot_phrases_path.clone())
            .add("my address", "221B Example Street", true)
            .unwrap();
        let mut other = NewEntry::new("dictate");
        other.final_text = "Untouched.".into();
        f.store.add(&other).unwrap();
        let entry = f.retryable();
        let before_rows = f.rows();
        let mut config = config();
        // Mail is an "email" app: its style drops a lone final period.
        config.set_field("style_email", serde_json::json!("casual"));
        let config_bytes = std::fs::read(&f.root.config_path).ok();
        let fake = Fake::new(Say::text());
        let updated = f.retry(&config, &entry, &fake).unwrap();

        // Parity: exactly what process_clip gives for the same inputs.
        let parity = Fake::new(Say::text());
        let speech = parity.open(&config).unwrap();
        let app = AppInfo {
            name: "Mail".into(),
            bundle_id: "com.apple.mail".into(),
        };
        let vocab = vocabulary(&f.root, &config);
        assert_eq!(vocab.hot_phrases.len(), 1);
        let (samples, _) = f
            .store
            .load_clip(Path::new(entry.audio_path.as_deref().unwrap()))
            .unwrap();
        let expected = process_clip(
            &samples,
            &config,
            &Models {
                speech: speech.as_ref(),
                llm: None,
            },
            &vocab,
            Some(&app),
        )
        .unwrap();
        assert_eq!(
            (updated.raw_text.clone(), updated.final_text.clone()),
            expected
        );
        assert_eq!(updated.final_text, "the quarterly review moved to thursday");
        let prompts = fake.counts.prompts.lock().unwrap().clone();
        assert!(prompts[0].as_deref().unwrap().contains("LocalFlow"));
        assert_eq!(fake.opens(), 1);
        assert_eq!(
            fake.counts.unloads.load(Ordering::SeqCst),
            1,
            "unloaded after use"
        );

        // Only status/raw/final/words of that row changed; nothing added.
        assert_eq!(updated.status, "ok");
        assert_eq!(updated.words, 6);
        let after_rows = f.rows();
        assert_eq!(after_rows.len(), before_rows.len());
        for (before, after) in before_rows.iter().zip(&after_rows) {
            if before.id == entry.id {
                assert_eq!(after, &updated);
                let restored = HistoryEntry {
                    status: before.status.clone(),
                    raw_text: before.raw_text.clone(),
                    final_text: before.final_text.clone(),
                    words: before.words,
                    ..after.clone()
                };
                assert_eq!(&restored, before);
            } else {
                assert_eq!(after, before);
            }
        }
        // The clip stays (no pruning); no config write, no last.txt.
        assert!(Path::new(updated.audio_path.as_deref().unwrap()).is_file());
        assert_eq!(std::fs::read(&f.root.config_path).ok(), config_bytes);
        assert!(!f.root.localflow_dir.join("last.txt").exists());
    }

    /// The microphone `sample_rate` (an old Python setting such as 8000 or
    /// 44100) doesn't describe saved clips, which are always 16 kHz: Retry
    /// is offered and runs with the clip's rate, and the user's config is
    /// neither changed in memory nor on disk.
    #[test]
    fn a_16k_clip_retries_whatever_the_microphone_sample_rate_is() {
        for capture_rate in [8_000, 44_100] {
            let f = fixture();
            let entry = f.retryable();
            let mut config = config();
            config.set_field("sample_rate", serde_json::json!(capture_rate));
            config.set_field("cleanup_enabled", serde_json::json!(true));
            config.set_field("language", serde_json::json!("de"));
            lf_core::config::save_config(&config, Some(&f.root.config_path)).unwrap();
            let config_bytes = std::fs::read(&f.root.config_path).unwrap();
            let before = config.clone();
            let fake = Fake::new(Says::Text("hello there"));

            let shown = view(&entry, &fake, &RetryJobs::new());
            assert!(shown.blocked.is_none(), "{shown:?}");
            let updated = f.retry(&config, &entry, &fake).unwrap();
            assert_eq!(updated.status, "ok");
            assert_eq!(updated.raw_text, "hello there");
            // The backend (speech and cleanup) saw the clip's actual rate.
            assert_eq!(
                *fake.counts.rates.lock().unwrap(),
                vec![Some(i64::from(CLIP_RATE)); 2]
            );
            assert_eq!(fake.counts.transcribes.load(Ordering::SeqCst), 1);
            // Only that one field differs in the retry's copy.
            let mut seen = clip_config(&config, CLIP_RATE);
            seen.set_field("sample_rate", serde_json::json!(capture_rate));
            assert_eq!(seen.fields, before.fields);
            // The caller's config and the file are untouched.
            assert_eq!(config.fields, before.fields);
            assert_eq!(config.field_i64("sample_rate"), Some(capture_rate));
            assert_eq!(std::fs::read(&f.root.config_path).unwrap(), config_bytes);
        }
    }

    #[test]
    fn a_failing_cleanup_provider_keeps_the_pipelines_fail_soft_text() {
        let f = fixture();
        let entry = f.retryable();
        let mut config = config();
        config.set_field("cleanup_enabled", serde_json::json!(true));
        config.set_field("cleanup_level", serde_json::json!("medium"));
        let mut fake = Fake::new(Says::Text("one two three four five six"));
        fake.failing_llm = true;
        let updated = f.retry(&config, &entry, &fake).unwrap();
        assert_eq!(fake.counts.llms.load(Ordering::SeqCst), 1);
        assert_eq!(updated.raw_text, "one two three four five six");
        assert_eq!(updated.final_text, "one two three four five six");
    }

    #[test]
    fn errors_silence_and_empty_text_leave_the_whole_row_untouched() {
        let f = fixture();
        let entry = f.retryable();
        let mut unavailable = Fake::new(Say::text());
        unavailable.open_error = Some("the Whisper base model isn't downloaded");
        let cases = [
            (
                unavailable,
                RetryError::Unavailable("the Whisper base model isn't downloaded".into()),
            ),
            (
                Fake::new(Says::Fails),
                RetryError::Failed("fake inference failed".into()),
            ),
            (Fake::new(Says::Text("")), RetryError::Empty),
            (Fake::new(Says::Text("   ")), RetryError::Empty),
        ];
        for (fake, expected) in cases {
            assert_eq!(f.retry(&config(), &entry, &fake).unwrap_err(), expected);
            assert_eq!(f.store.get(entry.id).unwrap().unwrap(), entry);
            let opened = usize::from(fake.open_error.is_none());
            assert_eq!(
                fake.counts.unloads.load(Ordering::SeqCst),
                opened,
                "an opened model is unloaded on failure too"
            );
        }
        // Silent audio stops at the shared silence gate: no inference.
        let silent_clip = f.store.write_clip(&[0.0; 16_000], 16_000).unwrap();
        let silent = f.failed_row(Some(silent_clip.to_string_lossy().into()));
        let fake = Fake::new(Say::text());
        assert_eq!(
            f.retry(&config(), &silent, &fake).unwrap_err(),
            RetryError::Empty
        );
        assert_eq!(fake.counts.transcribes.load(Ordering::SeqCst), 0);
        assert_eq!(f.store.get(silent.id).unwrap().unwrap(), silent);
        assert!(silent_clip.is_file());
    }

    /// Run a retry whose inference is held at the gate, apply `meanwhile`,
    /// then let it finish.
    fn mid_flight(
        f: &Fixture,
        entry: &HistoryEntry,
        meanwhile: impl FnOnce(&Fixture),
    ) -> Result<HistoryEntry, RetryError> {
        let (fake, go, running) = Fake::gated(Say::text());
        let root = f.root.clone();
        let key = RetryKey::of(entry);
        let job = std::thread::spawn(move || {
            let result = retry_entry(&root, &config(), key, &fake);
            (result, fake.counts.unloads.load(Ordering::SeqCst))
        });
        running.recv_timeout(WAIT).unwrap();
        meanwhile(f);
        go.send(()).unwrap();
        let (result, unloads) = job.join().unwrap();
        assert_eq!(unloads, 1);
        result
    }

    #[test]
    fn concurrent_edit_delete_clear_and_reused_id_are_never_overwritten() {
        let f = fixture();
        // Concurrent edit (another retry, the CLI).
        let entry = f.retryable();
        let result = mid_flight(&f, &entry, |f| {
            f.store
                .update(
                    entry.id,
                    &EntryUpdate {
                        raw_text: Some("edited elsewhere".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
        });
        assert_eq!(result.unwrap_err(), RetryError::Changed);
        assert_eq!(
            f.store.get(entry.id).unwrap().unwrap().raw_text,
            "edited elsewhere"
        );
        // Deleted mid-flight: nothing is (re)created.
        let entry = f.retryable();
        let count = f.rows().len();
        let result = mid_flight(&f, &entry, |f| {
            f.store.delete(entry.id).unwrap();
        });
        assert_eq!(result.unwrap_err(), RetryError::Changed);
        assert!(f.store.get(entry.id).unwrap().is_none());
        assert_eq!(f.rows().len(), count - 1);
        // Cleared, then a new dictation reuses the id (its only row).
        let f = fixture();
        let entry = f.retryable();
        let mut reused = None;
        let result = mid_flight(&f, &entry, |f| {
            f.store.clear().unwrap();
            let mut new = NewEntry::new("dictate");
            new.final_text = "A newer dictation.".into();
            new.audio_path = entry.audio_path.clone();
            let id = f.store.add(&new).unwrap().unwrap();
            reused = f.store.get(id).unwrap();
        });
        let reused = reused.unwrap();
        assert_eq!(reused.id, entry.id, "the id was reused");
        assert_eq!(result.unwrap_err(), RetryError::Changed);
        assert_eq!(f.rows(), vec![reused]);
    }

    fn entry_for_jobs(f: &Fixture) -> (HistoryEntry, RetryKey) {
        let entry = f.retryable();
        let key = RetryKey::of(&entry);
        (entry, key)
    }

    #[test]
    fn one_job_at_a_time_survives_repeat_clicks_and_keeps_its_result() {
        let f = fixture();
        let (entry, key) = entry_for_jobs(&f);
        let (other, other_key) = entry_for_jobs(&f);
        let jobs = RetryJobs::new();
        let (fake, go, running) = Fake::gated(Say::text());
        let fake = Arc::new(fake);
        let counts = fake.counts.clone();
        assert!(jobs.start(f.root.clone(), config(), key, fake.clone()));
        running.recv_timeout(WAIT).unwrap();
        assert_eq!(jobs.running(), Some(key));
        assert!(
            !lf_ui::shutdown::wait_for_pending_writes(Duration::ZERO),
            "a close waits for it"
        );
        let downloads = crate::controllers::jobs::Downloads::new();
        assert_eq!(
            crate::controllers::shutdown::remaining_work(&downloads, &jobs, false),
            vec![crate::controllers::shutdown::RETRY_RUNNING]
        );
        let mine = view(&entry, fake.as_ref(), &jobs);
        assert!(mine.mine && mine.blocked.is_none() && mine.error.is_none());
        assert!(mine.note.contains("can't be cancelled"));
        let theirs = view(&other, fake.as_ref(), &jobs);
        assert!(!theirs.mine);
        assert_eq!(theirs.blocked.as_deref(), Some(OTHER_RUNNING));
        // Repeat clicks and another row are refused: no second model load.
        assert!(!jobs.start(f.root.clone(), config(), key, fake.clone()));
        assert!(!jobs.start(f.root.clone(), config(), other_key, fake.clone()));
        assert_eq!(counts.opens.load(Ordering::SeqCst), 1);
        go.send(()).unwrap();
        assert!(jobs.wait_idle(WAIT));
        let finished = jobs.last_for(&entry).unwrap();
        assert_eq!(finished.key, key);
        assert_eq!(finished.outcome.as_ref().unwrap().status, "ok");
        assert!(
            jobs.last_for(&other).is_none(),
            "never applied to another row"
        );
        assert!(f.store.get(other.id).unwrap().unwrap() == other);
        // Shared through clones (pages come and go).
        assert_eq!(jobs.clone().last().unwrap().seq, finished.seq);
    }

    #[test]
    fn panics_and_thread_failures_release_the_slot_and_the_pending_write() {
        let f = fixture();
        let (entry, key) = entry_for_jobs(&f);
        let jobs = RetryJobs::new();
        let fake = Arc::new(Fake::new(Says::Panics));
        assert!(jobs.start(f.root.clone(), config(), key, fake.clone()));
        assert!(jobs.wait_idle(WAIT));
        assert_eq!(
            jobs.last().unwrap().outcome.unwrap_err(),
            RetryError::Panicked
        );
        assert_eq!(
            fake.counts.unloads.load(Ordering::SeqCst),
            1,
            "unloaded while unwinding"
        );
        assert_eq!(f.store.get(entry.id).unwrap().unwrap(), entry);
        // The thread can't be created: the slot frees and the error is kept.
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        assert!(jobs.start_with(
            key,
            move || {
                flag.store(true, Ordering::SeqCst);
                Err(RetryError::Empty)
            },
            Box::new(|_body| Err(std::io::Error::other("no threads left"))),
        ));
        assert!(jobs.running().is_none());
        assert!(!ran.load(Ordering::SeqCst));
        assert!(matches!(
            jobs.last().unwrap().outcome,
            Err(RetryError::Start(m)) if m.contains("no threads left")
        ));
        // And a later job starts normally.
        assert!(jobs.start(
            f.root.clone(),
            config(),
            key,
            Arc::new(Fake::new(Say::text()))
        ));
        assert!(jobs.wait_idle(WAIT));
        assert!(jobs.last().unwrap().outcome.is_ok());
    }

    use crate::controllers::history::{self, Delivery, Listing, ReadTicket};

    /// `n` plain rows newer than anything `retryable()` creates.
    fn newer_rows(f: &Fixture, n: usize) {
        let base = chrono::Utc::now().timestamp() as f64 + 1000.0;
        for i in 0..n {
            let mut new = NewEntry::new("dictate");
            new.final_text = format!("Row {i}.");
            new.ts = Some(base + i as f64);
            f.store.add(&new).unwrap();
        }
    }

    fn run(jobs: &RetryJobs, f: &Fixture, entry: &HistoryEntry, says: &'static str) {
        let fake = Arc::new(Fake::new(Says::Text(says)));
        assert!(jobs.start(f.root.clone(), config(), RetryKey::of(entry), fake));
        assert!(jobs.wait_idle(WAIT));
    }

    #[test]
    fn reads_that_cross_a_retry_commit_are_reread_not_applied() {
        let f = fixture();
        let entry = f.retryable(); // oldest row: on the second page
        newer_rows(&f, 60);
        let jobs = RetryJobs::new();
        let mut seen = jobs.seq();
        let mut list = Listing::default();
        list.apply(history::load(&f.root, 1, "", 0, false).unwrap());
        assert_eq!(list.next_offset(), 50);

        // An append (offset 50) is read before the retry commits and
        // delivered after the observer applied the retried row.
        let append = ReadTicket {
            generation: 2,
            retry_seq: jobs.seq(),
        };
        let stale = history::load(&f.root, 2, "", 50, false).unwrap();
        assert!(stale.entries.iter().any(|e| e == &entry), "pre-commit row");
        run(&jobs, &f, &entry, "fresh text after retry");
        let observed = jobs.observe(&mut seen);
        assert_eq!(observed.updated.len(), 1);
        assert!(!list.replace(observed.updated[0].clone()), "not loaded yet");
        assert_eq!(append.judge(2, jobs.seq()), Delivery::Reread);
        // The page re-reads the same intent (offset 50, same query): an
        // append, so page one and its cursor are kept.
        list.apply(history::load(&f.root, 3, "", 50, false).unwrap());
        assert_eq!(list.entries.len(), 61);
        assert_eq!(list.next_offset(), 61);
        let row = list.find(entry.id).unwrap();
        assert_eq!(row.final_text, "fresh text after retry");
        assert_eq!(row.status, "ok");

        // A first-page search read across a second commit: same answer.
        let other = f.retryable();
        let search = ReadTicket {
            generation: 4,
            retry_seq: jobs.seq(),
        };
        run(&jobs, &f, &other, "second fresh text");
        assert_eq!(search.judge(4, jobs.seq()), Delivery::Reread);
        // Nothing crossed: apply. A superseded request (its rows or its
        // error) is dropped whatever it carries.
        let current = ReadTicket {
            generation: 5,
            retry_seq: jobs.seq(),
        };
        assert_eq!(current.judge(5, jobs.seq()), Delivery::Apply);
        assert_eq!(search.judge(5, jobs.seq()), Delivery::Obsolete);
        assert_eq!(current.judge(6, jobs.seq()), Delivery::Obsolete);
    }

    /// Rows saying `text` (raw and final), each with a saved clip, `ts`
    /// rising from `base`, oldest first in the result.
    fn texts(f: &Fixture, base: f64, texts: &[String]) -> Vec<HistoryEntry> {
        texts
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let mut new = NewEntry::new("dictate");
                new.status = "failed".into();
                new.raw_text = text.clone();
                new.final_text = text.clone();
                new.audio_path = Some(f.clip());
                new.ts = Some(base + i as f64);
                let id = f.store.add(&new).unwrap().unwrap();
                f.store.get(id).unwrap().unwrap()
            })
            .collect()
    }

    fn ids(rows: &[HistoryEntry]) -> Vec<i64> {
        rows.iter().map(|e| e.id).collect()
    }

    /// Page `list` to the end, as `Load more` would.
    fn page_out(f: &Fixture, list: &mut Listing, query: &str) {
        while list.more {
            let offset = list.next_offset();
            list.apply(history::load(&f.root, 9, query, offset, false).unwrap());
        }
    }

    fn authoritative(f: &Fixture, query: &str) -> Vec<i64> {
        ids(&f.store.recent(10_000, Some(query), 0).unwrap())
    }

    /// The reviewed failure: 51 matches, 50 loaded, a retry removes one
    /// loaded row from the search. The prefix reread keeps 50 loaded, the
    /// former 51st match is listed, and the selected row stays in detail.
    #[test]
    fn a_retry_that_leaves_a_search_neither_skips_nor_drops_the_selection() {
        let f = fixture();
        let base = chrono::Utc::now().timestamp() as f64 + 1000.0;
        let words: Vec<String> = (0..71)
            .map(|i| {
                if i % 4 == 3 {
                    format!("plain {i}")
                } else {
                    format!("needle {i}")
                }
            })
            .collect();
        texts(&f, base, &words);
        let before = authoritative(&f, "needle");
        assert!(before.len() > 51);
        let jobs = RetryJobs::new();
        let mut seen = jobs.seq();
        let mut list = Listing::default();
        list.apply(history::load(&f.root, 1, "needle", 0, false).unwrap());
        assert_eq!((list.entries.len(), list.next_offset()), (50, 50));
        let victim = list.entries[10].clone();
        let selected = Some(victim.id);

        run(&jobs, &f, &victim, "nothing to find here");
        let observed = jobs.observe(&mut seen);
        for entry in observed.updated {
            assert!(list.replace(entry), "in place first");
        }
        let read = history::after_retry("needle", list.next_offset(), None).unwrap();
        assert_eq!(read, history::Read::prefix(50));
        list.refresh(
            history::read(&f.root, 2, "needle", read, false).unwrap(),
            selected,
        );
        let expected: Vec<i64> = before
            .iter()
            .copied()
            .filter(|id| *id != victim.id)
            .collect();
        assert_eq!(ids(&list.entries), expected[..50], "the former 51st is in");
        assert_eq!(list.next_offset(), 50, "loaded progress kept");
        let shown = list.find(victim.id).expect("still in the detail pane");
        assert_eq!(shown.final_text, "nothing to find here");
        assert!(!list.in_pages(victim.id), "cited, not listed");
        page_out(&f, &mut list, "needle");
        assert_eq!(ids(&list.entries), expected, "nothing skipped or twice");
        assert_eq!(expected, authoritative(&f, "needle"));
    }

    /// A retry that adds a match newer than the loaded prefix: it is listed
    /// at its place, later pages still line up, the selection is unchanged.
    #[test]
    fn a_retry_that_joins_a_search_before_the_loaded_prefix_is_listed() {
        let f = fixture();
        let base = chrono::Utc::now().timestamp() as f64 + 1000.0;
        let mut words: Vec<String> = (0..52).map(|i| format!("needle {i}")).collect();
        words.push("plain newest".into());
        let rows = texts(&f, base, &words);
        let joiner = rows.last().unwrap().clone();
        let jobs = RetryJobs::new();
        let mut seen = jobs.seq();
        let mut list = Listing::default();
        list.apply(history::load(&f.root, 1, "needle", 0, false).unwrap());
        assert!(list.more && !list.in_pages(joiner.id));
        let selected = list.entries[20].id;

        run(&jobs, &f, &joiner, "a needle after all");
        for entry in jobs.observe(&mut seen).updated {
            assert!(!list.replace(entry), "not listed yet");
        }
        let read = history::after_retry("needle", list.next_offset(), None).unwrap();
        list.refresh(
            history::read(&f.root, 2, "needle", read, false).unwrap(),
            Some(selected),
        );
        assert_eq!(list.entries[0].id, joiner.id);
        assert_eq!(list.entries[0].final_text, "a needle after all");
        assert_eq!(list.next_offset(), 50);
        assert!(list.in_pages(selected));
        page_out(&f, &mut list, "needle");
        let expected = authoritative(&f, "needle");
        assert_eq!(expected.len(), 53);
        assert_eq!(ids(&list.entries), expected);
    }

    /// A filtered append in flight when a retry commits never restores its
    /// offset: the observer supersedes it with a reread of everything it
    /// would have loaded, and if it is delivered first, its fence does the
    /// same. Unfiltered reads keep the in-place path.
    #[test]
    fn a_filtered_append_crossing_a_retry_rereads_its_whole_prefix() {
        let f = fixture();
        let base = chrono::Utc::now().timestamp() as f64 + 1000.0;
        let words: Vec<String> = (0..130).map(|i| format!("needle {i}")).collect();
        texts(&f, base, &words);
        let jobs = RetryJobs::new();
        let mut seen = jobs.seq();
        let mut list = Listing::default();
        list.apply(history::load(&f.root, 1, "needle", 0, false).unwrap());
        let append = history::Read::page(list.next_offset());
        let ticket = ReadTicket {
            generation: 2,
            retry_seq: jobs.seq(),
        };
        let victim = list.entries[3].clone();
        run(&jobs, &f, &victim, "gone from the search");

        // Delivered before the observer looks: its fence rereads the prefix.
        assert_eq!(ticket.judge(2, jobs.seq()), Delivery::Reread);
        let again = append.again("needle");
        assert_eq!(again, history::Read::prefix(100));
        // Or the observer looks first: the same read, superseding it.
        assert_eq!(
            history::after_retry("needle", list.next_offset(), Some(append)),
            Some(again)
        );
        assert_eq!(ticket.judge(3, jobs.seq()), Delivery::Obsolete);
        for entry in jobs.observe(&mut seen).updated {
            assert!(list.replace(entry));
        }
        list.refresh(
            history::read(&f.root, 3, "needle", again, false).unwrap(),
            Some(victim.id),
        );
        assert_eq!(list.next_offset(), 100, "the append's progress");
        assert_eq!(
            list.find(victim.id).unwrap().final_text,
            "gone from the search"
        );
        // A prefix reread that is itself crossed keeps its length.
        assert_eq!(again.again("needle"), again);
        page_out(&f, &mut list, "needle");
        assert_eq!(ids(&list.entries), authoritative(&f, "needle"));
        assert_eq!(list.entries.len(), 129);

        // Unfiltered: replaced in place, an append stays an append, and a
        // new list or search in flight is left to its own fence.
        assert_eq!(history::after_retry("  ", 50, None), None);
        assert_eq!(append.again(""), append);
        assert_eq!(
            history::after_retry("needle", 50, Some(history::Read::page(0))),
            None
        );
    }

    #[test]
    fn completions_before_an_observation_are_each_applied_exactly_once() {
        let f = fixture();
        let cited_entry = f.retryable(); // oldest: beyond the first page
        newer_rows(&f, 55);
        let paged_entry = {
            let mut new = NewEntry::new("dictate");
            new.status = "failed".into();
            new.audio_path = Some(f.clip());
            new.ts = Some(chrono::Utc::now().timestamp() as f64 + 5000.0);
            let id = f.store.add(&new).unwrap().unwrap();
            f.store.get(id).unwrap().unwrap() // newest: first page
        };
        let jobs = RetryJobs::new();
        let mut seen = jobs.seq();
        let mut list = Listing::default();
        list.apply(history::load(&f.root, 1, "", 0, false).unwrap());
        list.cite(cited_entry.clone());
        let order: Vec<i64> = list.entries.iter().map(|e| e.id).collect();
        // Two retries finish before the page looks.
        run(&jobs, &f, &paged_entry, "first retried");
        run(&jobs, &f, &cited_entry, "second retried");
        let observed = jobs.observe(&mut seen);
        assert!(!observed.missed);
        let texts: Vec<&str> = observed
            .updated
            .iter()
            .map(|e| e.final_text.as_str())
            .collect();
        assert_eq!(texts, ["first retried", "second retried"]);
        for entry in observed.updated {
            assert!(list.replace(entry));
        }
        assert_eq!(
            list.find(paged_entry.id).unwrap().final_text,
            "first retried"
        );
        assert_eq!(
            list.find(cited_entry.id).unwrap().final_text,
            "second retried"
        );
        assert!(!list.in_pages(cited_entry.id), "still only cited");
        assert_eq!(list.entries.iter().map(|e| e.id).collect::<Vec<_>>(), order);
        assert_eq!(list.next_offset(), 50);
        assert_eq!(jobs.observe(&mut seen), Observed::default(), "no repeats");
        // The error banner still follows only the latest job's row.
        assert!(jobs.last_for(&cited_entry).is_some());
        assert!(jobs.last_for(&paged_entry).is_none());
    }

    #[test]
    fn a_completion_right_after_an_observation_keeps_polling_and_is_delivered() {
        let f = fixture();
        let entry = f.retryable();
        let jobs = RetryJobs::new();
        let mut seen = jobs.seq();
        // Hold the job body instead of running it, to finish it on cue.
        type Body = Box<dyn FnOnce() + Send>;
        let held: Arc<Mutex<Option<Body>>> = Arc::default();
        let slot = held.clone();
        let updated = HistoryEntry {
            final_text: "delivered".into(),
            ..entry.clone()
        };
        let result = updated.clone();
        assert!(jobs.start_with(
            RetryKey::of(&entry),
            move || Ok(result),
            Box::new(move |body| {
                *slot.lock().unwrap() = Some(body);
                Ok(())
            }),
        ));
        let first = jobs.observe(&mut seen);
        assert!(first.running && first.updated.is_empty(), "poll again");
        // The worker finishes right after that observation.
        (held.lock().unwrap().take().unwrap())();
        assert!(jobs.running().is_none());
        let next = jobs.observe(&mut seen);
        assert_eq!(next.updated, vec![updated]);
        assert!(!next.running && !next.missed, "now the watcher may stop");
        assert_eq!(jobs.observe(&mut seen), Observed::default());
    }

    #[test]
    fn retained_results_are_bounded_and_an_overrun_observer_reloads() {
        let jobs = RetryJobs::new();
        let key = RetryKey { id: 1, ts: 1.0 };
        let mut early = 0; // never looked: misses the dropped first result
        let mut late = 0;
        for i in 0..=KEPT_RESULTS {
            assert!(jobs.start_with(
                key,
                || Err(RetryError::Empty),
                Box::new(|body| {
                    body();
                    Ok(())
                }),
            ));
            if i == KEPT_RESULTS - 1 {
                assert!(!jobs.observe(&mut late).missed, "all still kept");
            }
        }
        assert_eq!(jobs.state().finished.len(), KEPT_RESULTS);
        assert!(!jobs.observe(&mut late).missed);
        assert!(jobs.observe(&mut early).missed, "dropped results: reload");
        assert_eq!(early, jobs.seq());
    }

    #[test]
    fn a_failure_stays_on_its_row_with_try_again_until_retried() {
        let f = fixture();
        let (entry, key) = entry_for_jobs(&f);
        let (other, _) = entry_for_jobs(&f);
        let jobs = RetryJobs::new();
        let fake = Arc::new(Fake::new(Says::Fails));
        assert!(jobs.start(f.root.clone(), config(), key, fake.clone()));
        assert!(jobs.wait_idle(WAIT));
        let shown = view(&entry, fake.as_ref(), &jobs);
        assert!(shown.error.unwrap().contains("fake inference failed"));
        assert!(shown.blocked.is_none(), "Try again is usable");
        assert!(view(&other, fake.as_ref(), &jobs).error.is_none());
        // A reused id (same id, other timestamp) doesn't inherit the error.
        let mut reused = entry.clone();
        reused.ts += 1.0;
        assert!(view(&reused, fake.as_ref(), &jobs).error.is_none());
    }

    #[test]
    fn default_builds_explain_why_and_never_open_anything() {
        let f = fixture();
        let entry = f.retryable();
        let shown = view(&entry, &UnavailableRetry, &RetryJobs::new());
        assert!(shown
            .blocked
            .as_deref()
            .unwrap()
            .contains("--features runtime"));
        assert_eq!(shown.note, shown.blocked.unwrap());
        let reason = UnavailableRetry.unavailable().unwrap();
        assert!(reason.contains("--features runtime"));
        assert_eq!(
            retry_entry(&f.root, &config(), RetryKey::of(&entry), &UnavailableRetry),
            Err(RetryError::Unavailable(reason))
        );
        assert_eq!(f.store.get(entry.id).unwrap().unwrap(), entry);
    }

    #[cfg(unix)]
    mod python_fallback {
        use std::collections::HashMap;
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt as _;
        use std::path::PathBuf;

        use super::*;
        use crate::controllers::retry_python::PYTHON_ENV;
        use crate::data::Mode;

        // A script written while another test forks can fail to exec with
        // ETXTBSY, so the tests that write and run scripts take turns.
        static SCRIPTS: Mutex<()> = Mutex::new(());

        fn serial() -> MutexGuard<'static, ()> {
            SCRIPTS.lock().unwrap_or_else(|e| e.into_inner())
        }

        const MISSING: &str = "the Whisper base model isn't downloaded \
            (ggerganov/whisper.cpp/ggml-base.bin). Download it in Models & keys, then try again.";

        /// A fake Python `lf` (or interpreter) at `<dir>/<name>`: records its
        /// argv and environment, prints `stdout`/`stderr`, exits `code` (or
        /// hangs).
        struct FakeLf {
            dir: PathBuf,
            exe: PathBuf,
        }

        impl FakeLf {
            fn new(dir: &Path, name: &str) -> Self {
                std::fs::create_dir_all(dir).unwrap();
                let exe = dir.join(name);
                let here = dir.display();
                let script = format!(
                    "#!/bin/sh\nhere='{here}'\n\
                     {{ for a in \"$@\"; do printf 'arg=%s\\n' \"$a\"; done\n\
                     printf 'LOCALFLOW_CONFIG=%s\\nHF_HUB_OFFLINE=%s\\nPWD=%s\\nPID=%s\\n' \
                     \"$LOCALFLOW_CONFIG\" \"$HF_HUB_OFFLINE\" \"$(pwd)\" \"$$\"; }} > \"$here/record.tmp\"\n\
                     mv \"$here/record.tmp\" \"$here/record\"\n\
                     cat \"$here/stdout\"\ncat \"$here/stderr\" >&2\n\
                     if [ -f \"$here/hang\" ]; then exec sleep 30; fi\n\
                     exit \"$(cat \"$here/code\")\"\n"
                );
                std::fs::write(&exe, script).unwrap();
                std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
                let fake = FakeLf {
                    dir: dir.to_path_buf(),
                    exe,
                };
                fake.reply(0, "", "");
                fake
            }
            fn reply(&self, code: i32, stdout: &str, stderr: &str) {
                std::fs::write(self.dir.join("code"), code.to_string()).unwrap();
                std::fs::write(self.dir.join("stdout"), stdout).unwrap();
                std::fs::write(self.dir.join("stderr"), stderr).unwrap();
            }
            fn hang(&self) {
                std::fs::write(self.dir.join("hang"), "").unwrap();
            }
            fn record(&self) -> Option<Vec<String>> {
                let text = std::fs::read_to_string(self.dir.join("record")).ok()?;
                Some(text.lines().map(String::from).collect())
            }
            fn value(&self, name: &str) -> String {
                let prefix = format!("{name}=");
                self.record()
                    .unwrap()
                    .iter()
                    .find_map(|line| line.strip_prefix(&prefix).map(String::from))
                    .unwrap()
            }
            fn args(&self) -> Vec<String> {
                self.record()
                    .unwrap()
                    .iter()
                    .filter_map(|line| line.strip_prefix("arg=").map(String::from))
                    .collect()
            }
        }

        /// A real-mode window's data, in a directory whose path has spaces.
        fn real_fixture() -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let mut root = DataRoot::sandbox(&dir.path().join("data root"));
            root.mode = Mode::Real;
            root.sandbox_dir = None;
            let store = open_history(&root).unwrap();
            Fixture {
                _dir: dir,
                root,
                store,
            }
        }

        /// A backend whose native model isn't downloaded, with the Python
        /// fallback seeing only `env`.
        fn missing_native(env: &[(&str, &Path)], timeout: Duration) -> Fake {
            let env: HashMap<String, OsString> = env
                .iter()
                .map(|(name, value)| (name.to_string(), value.as_os_str().to_owned()))
                .collect();
            let mut fake = Fake::new(Say::text());
            fake.open_error = Some(MISSING);
            fake.python = Some(PythonRetry::with(
                Box::new(move |name| env.get(name).cloned()),
                timeout,
            ));
            fake
        }

        /// What `lf history retry --json` prints (`ts` as Python's `repr`,
        /// which is also Rust's shortest round-trip form).
        fn reply_for(entry: &HistoryEntry, raw: &str, text: &str) -> String {
            serde_json::json!({
                "id": entry.id,
                "ts": format!("{:?}", entry.ts),
                "raw_text": raw,
                "final_text": text,
            })
            .to_string()
        }

        fn unchanged(f: &Fixture, before: &[HistoryEntry]) {
            assert_eq!(f.rows(), before);
        }

        #[test]
        fn a_missing_native_model_retries_with_this_windows_python_app() {
            let _serial = serial();
            let f = real_fixture();
            let mut other = NewEntry::new("dictate");
            other.final_text = "Untouched.".into();
            f.store.add(&other).unwrap();
            let entry = f.retryable();
            let before = f.rows();
            let config_bytes = std::fs::read(&f.root.config_path).ok();
            let python = FakeLf::new(&f._dir.path().join("python env"), "python3");
            python.reply(
                0,
                &format!(
                    "python raw  (12ms)\n{}\n",
                    reply_for(&entry, "python raw", "Python final.")
                ),
                "a warning on stderr\n",
            );
            let fake = missing_native(&[(PYTHON_ENV, &python.exe)], WAIT);

            let updated = f.retry(&config(), &entry, &fake).unwrap();
            assert_eq!(updated.status, "ok");
            assert_eq!(updated.raw_text, "python raw");
            assert_eq!(updated.final_text, "Python final.");
            // The interpreter the launcher exported runs this window's
            // data directory and config, offline.
            let data_dir = f.root.localflow_dir.to_string_lossy().into_owned();
            assert_eq!(
                python.args(),
                [
                    "-m",
                    "localflow.cli",
                    "history",
                    "retry",
                    &entry.id.to_string(),
                    "--json",
                    "--data-dir",
                    &data_dir,
                ]
            );
            assert_eq!(
                python.value("LOCALFLOW_CONFIG"),
                f.root.config_path.to_string_lossy()
            );
            assert_eq!(python.value("HF_HUB_OFFLINE"), "1");
            assert_eq!(
                PathBuf::from(python.value("PWD")).canonicalize().unwrap(),
                f.root.localflow_dir.canonicalize().unwrap()
            );
            // The native model was only asked for, never used.
            assert_eq!(fake.opens(), 1);
            assert_eq!(fake.counts.transcribes.load(Ordering::SeqCst), 0);
            assert_eq!(fake.counts.llms.load(Ordering::SeqCst), 0);
            // Only that row changed; no config write, no last.txt.
            let after = f.rows();
            assert_eq!(after.len(), before.len());
            for (before, after) in before.iter().zip(&after) {
                if before.id == entry.id {
                    assert_eq!(after, &updated);
                } else {
                    assert_eq!(after, before);
                }
            }
            assert_eq!(std::fs::read(&f.root.config_path).ok(), config_bytes);
            assert!(!f.root.localflow_dir.join("last.txt").exists());
            // The page reloads it through the job slot like a native result.
            let jobs = RetryJobs::new();
            let mut seen = jobs.seq();
            let second = f.retryable();
            python.reply(0, &reply_for(&second, "again", "Again."), "");
            assert!(jobs.start(
                f.root.clone(),
                config(),
                RetryKey::of(&second),
                Arc::new(missing_native(&[(PYTHON_ENV, &python.exe)], WAIT)),
            ));
            assert!(jobs.wait_idle(WAIT));
            let observed = jobs.observe(&mut seen);
            assert_eq!(observed.updated.len(), 1);
            assert_eq!(observed.updated[0].final_text, "Again.");
            assert_eq!(
                f.store.get(second.id).unwrap().unwrap(),
                observed.updated[0]
            );
        }

        #[test]
        fn without_the_launcher_an_lf_on_path_is_used_but_never_lf_rs() {
            let _serial = serial();
            let f = real_fixture();
            let entry = f.retryable();
            let base = f._dir.path();
            let rs = FakeLf::new(&base.join("rust bin"), "lf-rs");
            std::os::unix::fs::symlink(&rs.exe, base.join("rust bin").join("lf")).unwrap();
            let lf = FakeLf::new(&base.join("python bin"), "lf");
            lf.reply(0, &reply_for(&entry, "raw", "From PATH."), "");
            let path = std::env::join_paths([
                PathBuf::from("relative"),
                base.join("rust bin"),
                base.join("python bin"),
            ])
            .unwrap();
            let fake = missing_native(&[("PATH", Path::new(&path))], WAIT);
            assert_eq!(
                f.retry(&config(), &entry, &fake).unwrap().final_text,
                "From PATH."
            );
            assert_eq!(lf.args()[..2], ["history", "retry"]);
            assert!(rs.record().is_none(), "lf-rs has no faster-whisper");
            // A relative or non-file override falls back to PATH as well.
            let entry = f.retryable();
            lf.reply(0, &reply_for(&entry, "raw", "Still PATH."), "");
            let fake = missing_native(
                &[
                    (PYTHON_ENV, Path::new("python3")),
                    ("PATH", Path::new(&path)),
                ],
                WAIT,
            );
            assert_eq!(
                f.retry(&config(), &entry, &fake).unwrap().final_text,
                "Still PATH."
            );
        }

        #[test]
        fn an_installed_native_model_never_runs_python() {
            let _serial = serial();
            let f = real_fixture();
            let entry = f.retryable();
            let python = FakeLf::new(&f._dir.path().join("py"), "python3");
            let mut fake = missing_native(&[(PYTHON_ENV, &python.exe)], WAIT);
            fake.open_error = None;
            let updated = f.retry(&config(), &entry, &fake).unwrap();
            assert_eq!(updated.raw_text, "teh quarterly review moved to thursday.");
            assert!(python.record().is_none());
            // Other native errors aren't a missing model: no fallback either.
            fake.open_error = Some("the speech helper crashed");
            assert_eq!(
                f.retry(&config(), &f.retryable(), &fake).unwrap_err(),
                RetryError::Unavailable("the speech helper crashed".into())
            );
            assert!(python.record().is_none());
        }

        #[test]
        fn the_sandbox_never_runs_python() {
            let _serial = serial();
            let f = fixture();
            assert!(f.root.is_sandbox());
            let entry = f.retryable();
            let python = FakeLf::new(&f._dir.path().join("py"), "python3");
            python.reply(0, &reply_for(&entry, "raw", "Real text."), "");
            let fake = missing_native(&[(PYTHON_ENV, &python.exe)], WAIT);
            assert_eq!(
                f.retry(&config(), &entry, &fake).unwrap_err(),
                RetryError::Unavailable(MISSING.into())
            );
            assert!(python.record().is_none());
            assert_eq!(f.store.get(entry.id).unwrap().unwrap(), entry);
        }

        #[test]
        fn a_failing_python_app_is_one_short_sentence_and_changes_nothing() {
            let _serial = serial();
            let f = real_fixture();
            let entry = f.retryable();
            let before = f.rows();
            let python = FakeLf::new(&f._dir.path().join("py"), "python3");
            let fake = missing_native(&[(PYTHON_ENV, &python.exe)], WAIT);
            let traceback = "retry failed for history entry 1\n\
                Traceback (most recent call last):\n  File \"app.py\", line 9, in retry\n\
                ValueError: audio clip must be mono 16-bit PCM\n";
            let cases = [
                (
                    format!("{traceback}Error: audio clip must be mono 16-bit PCM\n"),
                    "Transcription failed: the Python app reported: audio clip must be mono 16-bit PCM.",
                ),
                (
                    traceback.into(),
                    "Transcription failed: the Python app reported: ValueError: audio clip must be mono 16-bit PCM.",
                ),
                (String::new(), "Transcription failed: the Python app exited with code 1."),
            ];
            for (stderr, expected) in cases {
                python.reply(1, "partial output\n", &stderr);
                let error = f.retry(&config(), &entry, &fake).unwrap_err();
                assert_eq!(error.message(), expected);
                assert!(!error.message().contains("Traceback"));
                unchanged(&f, &before);
            }
            let long = format!("Error: {}\n", "word ".repeat(100));
            python.reply(1, "", &long);
            let message = f.retry(&config(), &entry, &fake).unwrap_err().message();
            assert!(
                message.chars().count() < 220 && message.ends_with('…'),
                "{message}"
            );
            // A reply about another row (another data location) or with no
            // speech is never saved.
            let mut elsewhere = entry.clone();
            elsewhere.ts += 1.0;
            python.reply(0, &reply_for(&elsewhere, "raw", "Other."), "");
            assert!(matches!(
                f.retry(&config(), &entry, &fake),
                Err(RetryError::Failed(m)) if m.contains("different history")
            ));
            python.reply(0, &reply_for(&entry, "", ""), "");
            assert_eq!(
                f.retry(&config(), &entry, &fake).unwrap_err(),
                RetryError::Empty
            );
            unchanged(&f, &before);
        }

        #[test]
        fn no_python_app_or_model_names_both_ways_out() {
            let _serial = serial();
            let f = real_fixture();
            let entry = f.retryable();
            let before = f.rows();
            let both_ways = |reason: &str| {
                RetryError::Unavailable(format!(
                    "the Whisper base model isn't downloaded (ggerganov/whisper.cpp/ggml-base.bin), \
                     and {reason}. Download it in Models & keys, or download the Python app's \
                     model with `lf models download`, then try again."
                ))
            };
            let nowhere = missing_native(&[], WAIT);
            assert_eq!(
                f.retry(&config(), &entry, &nowhere).unwrap_err(),
                both_ways("the Python LocalFlow app wasn't found")
            );
            let python = FakeLf::new(&f._dir.path().join("py"), "python3");
            let fake = missing_native(&[(PYTHON_ENV, &python.exe)], WAIT);
            python.reply(3, "", "Error: the fw-base speech model isn't downloaded\n");
            assert_eq!(
                f.retry(&config(), &entry, &fake).unwrap_err(),
                both_ways("the Python app's model isn't downloaded either")
            );
            // An `lf` without `--json` (lf-rs, an older Python app) or one
            // that prints no result.
            for (code, stdout) in [(2, ""), (0, "Retried dictation and copied the result.\n")] {
                python.reply(code, stdout, "Error: No such option: --json\n");
                assert_eq!(
                    f.retry(&config(), &entry, &fake).unwrap_err(),
                    both_ways("the `lf` found isn't a Python LocalFlow app that can retry")
                );
            }
            unchanged(&f, &before);
        }

        #[cfg(feature = "runtime")]
        #[test]
        fn the_unsupported_size_wording_is_lf_stts() {
            assert_eq!(NO_NATIVE_SIZE, lf_stt::engines::UNSUPPORTED_WHISPER_SIZE);
        }

        #[test]
        fn a_size_only_python_has_retries_with_the_python_app() {
            let _serial = serial();
            let f = real_fixture();
            let mut config = config();
            config.set_field("model_size", serde_json::json!("distil-large-v3"));
            let no_size = "unsupported whisper.cpp model size: distil-large-v3";
            let entry = f.retryable();
            let python = FakeLf::new(&f._dir.path().join("py"), "python3");
            python.reply(0, &reply_for(&entry, "raw", "Distil."), "");
            let mut fake = missing_native(&[(PYTHON_ENV, &python.exe)], WAIT);
            fake.open_error = Some(no_size);
            let updated = f.retry(&config, &entry, &fake).unwrap();
            assert_eq!(updated.final_text, "Distil.");
            assert_eq!(f.store.get(entry.id).unwrap().unwrap(), updated);

            // Without the Python app (or its model) the message says the
            // size is Python-only, not "download it in Models & keys".
            let entry = f.retryable();
            let before = f.rows();
            let python_only = |reason: &str| {
                RetryError::Unavailable(format!(
                    "The `distil-large-v3` speech model only runs in the Python LocalFlow app, \
                     and {reason}. Download the Python app's model with `lf models download`, \
                     or pick another model in Models & keys, then try again."
                ))
            };
            let mut nowhere = missing_native(&[], WAIT);
            nowhere.open_error = Some(no_size);
            assert_eq!(
                f.retry(&config, &entry, &nowhere).unwrap_err(),
                python_only("the Python LocalFlow app wasn't found")
            );
            python.reply(3, "", "Error: the speech model isn't downloaded\n");
            assert_eq!(
                f.retry(&config, &entry, &fake).unwrap_err(),
                python_only("it isn't downloaded there")
            );
            unchanged(&f, &before);

            // Other native errors (provider, language, a corrupt model)
            // never run Python.
            std::fs::remove_file(python.dir.join("record")).unwrap();
            for error in [
                "unsupported whisper.cpp provider: cuda",
                "unsupported Whisper language: xx",
                "failed to load the Whisper model: invalid model file",
            ] {
                fake.open_error = Some(error);
                assert_eq!(
                    f.retry(&config, &entry, &fake).unwrap_err(),
                    RetryError::Unavailable(error.into())
                );
            }
            assert!(python.record().is_none());
            unchanged(&f, &before);
        }

        #[test]
        fn a_python_lf_in_local_bin_is_found_off_path_but_never_lf_rs() {
            let _serial = serial();
            let f = real_fixture();
            let entry = f.retryable();
            let base = f._dir.path();
            let empty_path = base.join("empty bin");
            std::fs::create_dir_all(&empty_path).unwrap();
            // HOME/.local/bin/lf → lf-rs is skipped.
            let rs_home = base.join("rs home");
            let rs = FakeLf::new(&rs_home.join("rust"), "lf-rs");
            std::fs::create_dir_all(rs_home.join(".local/bin")).unwrap();
            std::os::unix::fs::symlink(&rs.exe, rs_home.join(".local/bin/lf")).unwrap();
            let fake = missing_native(&[("PATH", &empty_path), ("HOME", &rs_home)], WAIT);
            assert!(matches!(
                f.retry(&config(), &entry, &fake),
                Err(RetryError::Unavailable(m)) if m.contains("wasn't found")
            ));
            assert!(rs.record().is_none());
            // The Python CLI install.sh links there is used.
            let home = base.join("home");
            let lf = FakeLf::new(&home.join(".local/bin"), "lf");
            lf.reply(0, &reply_for(&entry, "raw", "From local bin."), "");
            let fake = missing_native(&[("PATH", &empty_path), ("HOME", &home)], WAIT);
            assert_eq!(
                f.retry(&config(), &entry, &fake).unwrap().final_text,
                "From local bin."
            );
            assert_eq!(lf.args()[..2], ["history", "retry"]);
        }

        #[test]
        fn a_relative_config_path_reaches_python_absolute() {
            let _serial = serial();
            let mut f = real_fixture();
            f.root.config_path = PathBuf::from("relative dir/config.toml");
            let entry = f.retryable();
            let python = FakeLf::new(&f._dir.path().join("py"), "python3");
            python.reply(0, &reply_for(&entry, "raw", "Text."), "");
            let fake = missing_native(&[(PYTHON_ENV, &python.exe)], WAIT);
            f.retry(&config(), &entry, &fake).unwrap();
            assert_eq!(
                PathBuf::from(python.value("LOCALFLOW_CONFIG")),
                std::env::current_dir()
                    .unwrap()
                    .join("relative dir/config.toml")
            );
        }

        fn reaped(python: &FakeLf) {
            let pid = python.value("PID");
            if cfg!(target_os = "linux") {
                assert!(
                    !Path::new(&format!("/proc/{pid}")).exists(),
                    "the child was killed and reaped (no zombie)"
                );
            }
        }

        #[test]
        fn a_timeout_or_a_close_kills_and_reaps_the_child_and_changes_nothing() {
            let _serial = serial();
            let f = real_fixture();
            let entry = f.retryable();
            let before = f.rows();
            let python = FakeLf::new(&f._dir.path().join("py"), "python3");
            python.hang();
            let started = std::time::Instant::now();
            let fake = missing_native(&[(PYTHON_ENV, &python.exe)], Duration::from_millis(300));
            assert_eq!(
                f.retry(&config(), &entry, &fake).unwrap_err().message(),
                "Transcription failed: the Python app didn't finish within 300 ms"
            );
            assert!(started.elapsed() < Duration::from_secs(10));
            reaped(&python);
            unchanged(&f, &before);

            // A window close stops it mid-run.
            std::fs::remove_file(python.dir.join("record")).unwrap();
            let fake = Arc::new(missing_native(&[(PYTHON_ENV, &python.exe)], WAIT));
            let job = {
                let (root, fake) = (f.root.clone(), fake.clone());
                let key = RetryKey::of(&entry);
                std::thread::spawn(move || retry_entry(&root, &config(), key, fake.as_ref()))
            };
            let deadline = std::time::Instant::now() + WAIT;
            while python.record().is_none() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            fake.python().unwrap().stop();
            assert_eq!(
                job.join().unwrap().unwrap_err().message(),
                "Transcription failed: it was stopped because LocalFlow is closing"
            );
            reaped(&python);
            unchanged(&f, &before);
        }
    }
}
