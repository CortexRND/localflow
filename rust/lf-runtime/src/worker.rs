//! The worker thread and per-job model snapshots (`app._worker_loop`,
//! `app.RuntimeModels`). Model clients are rebuilt only between jobs, so a
//! running job always keeps the models it started with.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use lf_core::config::Config;
use lf_core::history::HistoryStore;
use lf_providers::LlmProvider;

use crate::controller::Job;
use crate::engine::SpeechEngine;
use crate::pipeline::{process_job, Models, Vocabulary};
use crate::status::write_last_text;

pub type LlmFactory = Arc<dyn Fn(&Config) -> Option<Box<dyn LlmProvider>> + Send + Sync>;
pub type SttFactory = Arc<dyn Fn(&Config) -> Result<Arc<dyn SpeechEngine>, String> + Send + Sync>;
pub type Deliver = Arc<dyn Fn(&Job, Option<String>, bool) + Send + Sync>;

pub enum WorkItem {
    Job(Job),
    /// The LLM group changed: rebuild the cleanup/transform client.
    ReloadLlm(Arc<Config>),
    /// The speech group changed: load the new model, keep the old on failure.
    ReloadStt(Arc<Config>),
    Shutdown,
}

pub struct Worker {
    pub config: Arc<dyn Fn() -> Arc<Config> + Send + Sync>,
    pub vocabulary: Arc<dyn Fn(&Config) -> Vocabulary + Send + Sync>,
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
    pub fn run(mut self, rx: Receiver<WorkItem>) {
        let mut llm = (self.build_llm)(&(self.config)());
        while let Ok(item) = rx.recv() {
            match item {
                WorkItem::Shutdown => return,
                WorkItem::ReloadLlm(config) => llm = (self.build_llm)(&config),
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
                WorkItem::Job(job) => self.process(job, llm.as_deref()),
            }
        }
    }

    fn process(&self, job: Job, llm: Option<&dyn LlmProvider>) {
        let config = (self.config)();
        let vocab = (self.vocabulary)(&config);
        let models = Models {
            speech: self.speech.as_ref(),
            llm,
        };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            process_job(&job, &config, &models, &vocab, self.history.as_ref())
        }));
        let (text, error) = outcome.unwrap_or_else(|_| {
            log::error!("worker failed to process job");
            (None, true)
        });
        if let (Some(text), Some(path)) = (&text, &self.last_text_path) {
            if let Err(e) = write_last_text(text, path) {
                log::error!("could not write last text: {e}");
            }
        }
        (self.deliver)(&job, text, error);
    }
}
