//! The worker thread and per-job model snapshots (`app._worker_loop`,
//! `app.RuntimeModels`). Model clients are rebuilt only between jobs, so a
//! running job always keeps the models it started with.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use lf_core::config::Config;
use lf_core::history::HistoryStore;
use lf_providers::LlmProvider;

use crate::controller::{Job, JobKind};
use crate::engine::SpeechEngine;
use crate::pipeline::{process_job_guarded, Models, Vocabulary};
use crate::status::write_last_text;

pub type LlmFactory = Arc<dyn Fn(&Config) -> Option<Box<dyn LlmProvider>> + Send + Sync>;
pub type SttFactory = Arc<dyn Fn(&Config) -> Result<Arc<dyn SpeechEngine>, String> + Send + Sync>;
pub type VocabularyFn = Arc<dyn Fn(&Config) -> Vocabulary + Send + Sync>;
pub type Deliver = Arc<dyn Fn(&Job, Option<String>, bool) + Send + Sync>;

static RESULT_COMMIT: Mutex<()> = Mutex::new(());

pub enum WorkItem {
    Job(Job),
    /// The LLM group changed: rebuild the cleanup/transform client.
    ReloadLlm(Arc<Config>),
    /// The speech group changed: load the new model, keep the old on failure.
    ReloadStt(Arc<Config>),
    /// A recording started: load the speech model now if it is not loaded,
    /// so the job that follows does not pay the load.
    PrepareSpeech,
    Shutdown,
}

pub struct Worker {
    pub config: Arc<dyn Fn() -> Arc<Config> + Send + Sync>,
    pub vocabulary: VocabularyFn,
    pub history: Option<HistoryStore>,
    pub speech: Arc<dyn SpeechEngine>,
    pub build_llm: LlmFactory,
    pub build_stt: SttFactory,
    pub deliver: Deliver,
    pub last_text_path: Option<PathBuf>,
}

impl Worker {
    /// Process work until shutdown. The LLM client is built here, on the
    /// worker thread that uses it.
    pub fn run(self, rx: Receiver<WorkItem>) {
        self.run_until(rx, Arc::new(AtomicBool::new(false)));
    }

    pub fn run_until(mut self, rx: Receiver<WorkItem>, stop: Arc<AtomicBool>) {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let config = (self.config)();
        let mut llm = (self.build_llm)(&config);
        let mut fast_llm = self.build_fast_llm(&config);
        while let Ok(item) = rx.recv() {
            if stop.load(Ordering::Acquire) {
                return;
            }
            match item {
                WorkItem::Shutdown => return,
                WorkItem::ReloadLlm(config) => {
                    llm = (self.build_llm)(&config);
                    fast_llm = self.build_fast_llm(&config);
                }
                WorkItem::ReloadStt(config) => match (self.build_stt)(&config) {
                    Ok(engine) => match engine.load() {
                        Ok(()) => {
                            let old = std::mem::replace(&mut self.speech, engine);
                            old.unload();
                            log::info!("speech model reloaded");
                        }
                        Err(e) => {
                            log::error!("speech model failed to load; keeping the old one: {e}")
                        }
                    },
                    Err(e) => {
                        log::error!("could not build the speech model; keeping the old one: {e}")
                    }
                },
                WorkItem::PrepareSpeech => {
                    if !self.speech.loaded() {
                        if let Err(e) = self.speech.load() {
                            log::warn!("speech model preload failed: {e}");
                        }
                    }
                }
                WorkItem::Job(job) => self.process(job, llm.as_deref(), fast_llm.as_deref(), &stop),
            }
        }
    }

    fn build_fast_llm(&self, config: &Config) -> Option<Box<dyn LlmProvider>> {
        config
            .fast_model_config()
            .and_then(|fast| (self.build_llm)(&fast))
    }

    fn process(
        &self,
        job: Job,
        llm: Option<&dyn LlmProvider>,
        fast_llm: Option<&dyn LlmProvider>,
        stop: &AtomicBool,
    ) {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let config = (self.config)();
        let vocab = (self.vocabulary)(&config);
        let llm = if job.kind == JobKind::Dictate && config.uses_light_cleanup() {
            fast_llm.or(llm)
        } else {
            llm
        };
        let models = Models {
            speech: self.speech.as_ref(),
            llm,
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            process_job_guarded(
                &job,
                &config,
                &models,
                &vocab,
                self.history.as_ref(),
                || {
                    let guard = RESULT_COMMIT
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    (!stop.load(Ordering::Acquire)).then_some(guard)
                },
            )
        }));
        let ((text, error), _commit) = outcome.unwrap_or_else(|_| {
            log::error!("worker failed to process job");
            ((None, true), None)
        });
        if stop.load(Ordering::Acquire) {
            return;
        }
        if let (Some(text), Some(path)) = (&text, &self.last_text_path) {
            if let Err(e) = write_last_text(text, path) {
                log::error!("could not write last text: {e}");
            }
        }
        drop(_commit);
        if stop.load(Ordering::Acquire) {
            return;
        }
        (self.deliver)(&job, text, error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lf_providers::{Health, ModelInfo, ProviderError};
    use serde_json::json;
    use std::sync::mpsc;
    use std::time::Duration;

    struct Speech {
        text: &'static str,
        gate: Option<(mpsc::Sender<()>, Mutex<mpsc::Receiver<()>>)>,
    }
    impl SpeechEngine for Speech {
        fn load(&self) -> Result<(), String> {
            Ok(())
        }
        fn unload(&self) {}
        fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
            if let Some((entered, release)) = &self.gate {
                entered.send(()).unwrap();
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
            }
            Ok(self.text.into())
        }
    }

    struct RecordingLlm(String, Arc<Mutex<Vec<String>>>);

    impl LlmProvider for RecordingLlm {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn offline(&self) -> bool {
            true
        }
        fn complete(&self, _: &str, _: &str, _: Option<i64>) -> Result<String, ProviderError> {
            self.1.lock().unwrap().push(self.0.clone());
            Ok("Cleaned text.".into())
        }
        fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            unreachable!()
        }
        fn healthcheck(&self) -> Health {
            unreachable!()
        }
    }

    #[test]
    fn only_light_dictation_uses_fast_model() {
        for (level, expected) in [
            ("light", vec!["main", "fast", "main", "main"]),
            ("medium", vec!["main", "main", "main", "main"]),
        ] {
            let mut config = Config::default();
            config.set_field("llm_model", json!("main"));
            config.set_field("cleanup_level", json!(level));
            let config = Arc::new(config);
            let calls = Arc::new(Mutex::new(Vec::new()));
            let capture = calls.clone();
            let initial = config.clone();
            let worker = Worker {
                config: Arc::new(move || initial.clone()),
                vocabulary: Arc::new(|_| Vocabulary::default()),
                history: None,
                speech: Arc::new(Speech {
                    text: "this transcript has enough words",
                    gate: None,
                }),
                build_llm: Arc::new(move |config| {
                    Some(Box::new(RecordingLlm(
                        config.field_str("llm_model").unwrap().into(),
                        capture.clone(),
                    )))
                }),
                build_stt: Arc::new(|_| unreachable!()),
                deliver: Arc::new(|_, _, error| assert!(!error)),
                last_text_path: None,
            };
            let job = Job {
                kind: JobKind::Dictate,
                audio: Some(vec![0.2; 16000]),
                app_info: None,
                selection: String::new(),
                preset: None,
                clipboard_before: None,
            };
            let mut updated = (*config).clone();
            updated.set_field("llm_fast_model", json!("fast"));
            let (tx, rx) = mpsc::channel();
            for item in [
                WorkItem::Job(job.clone()),
                WorkItem::ReloadLlm(Arc::new(updated)),
                WorkItem::Job(job.clone()),
                WorkItem::Job(Job {
                    kind: JobKind::Command,
                    ..job.clone()
                }),
                WorkItem::Job(Job {
                    kind: JobKind::Transform,
                    preset: Some("polish".into()),
                    selection: "rough text".into(),
                    ..job
                }),
                WorkItem::Shutdown,
            ] {
                tx.send(item).unwrap();
            }
            worker.run(rx);
            assert_eq!(*calls.lock().unwrap(), expected);
        }
    }

    #[test]
    fn stopped_inference_cannot_overwrite_restarted_history_or_paste_last() {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("history.db");
        let clips = root.path().join("clips");
        let last = root.path().join("last-text");
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let mut config = Config::default();
        config.set_field("cleanup_enabled", serde_json::json!(false));
        let config = Arc::new(config);
        let worker = |speech: Arc<dyn SpeechEngine>| {
            let (config, delivered) = (config.clone(), delivered.clone());
            Worker {
                config: Arc::new(move || config.clone()),
                vocabulary: Arc::new(|_| Vocabulary::default()),
                history: Some(
                    HistoryStore::open(Some(database.clone()), Some(clips.clone()), true).unwrap(),
                ),
                speech,
                build_llm: Arc::new(|_| None),
                build_stt: Arc::new(|_| unreachable!()),
                deliver: Arc::new(move |_, text, error| {
                    assert!(!error);
                    delivered.lock().unwrap().push(text);
                }),
                last_text_path: Some(last.clone()),
            }
        };
        let job = Job {
            kind: JobKind::Dictate,
            audio: Some(vec![0.2; 16000]),
            app_info: None,
            selection: String::new(),
            preset: None,
            clipboard_before: None,
        };
        let (entered, blocked) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let old = worker(Arc::new(Speech {
            text: "stale result",
            gate: Some((entered, Mutex::new(released))),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let old_job = job.clone();
        let old = std::thread::spawn(move || old.process(old_job, None, None, &stopped));
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        stop.store(true, Ordering::Release);
        worker(Arc::new(Speech {
            text: "new result",
            gate: None,
        }))
        .process(job, None, None, &AtomicBool::new(false));
        release.send(()).unwrap();
        old.join().unwrap();
        let history = HistoryStore::open(Some(database), Some(clips.clone()), true).unwrap();
        assert_eq!(history.latest_text("").unwrap(), "new result");
        assert_eq!(history.recent(100, None, 0).unwrap().len(), 1);
        assert_eq!(std::fs::read_to_string(last).unwrap().trim(), "new result");
        assert_eq!(*delivered.lock().unwrap(), vec![Some("new result".into())]);
        assert_eq!(std::fs::read_dir(clips).unwrap().count(), 1);
    }
}
