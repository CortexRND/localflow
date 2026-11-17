//! Optional composable runtime backend. No Python process or hardware is implicit.
mod providers;
mod session;
mod validation;
mod workprompts;

pub use providers::{ConfiguredProviders, ProviderFactory, TitleLlm};
pub use session::{CaptureFactory, UnavailableCapture};

use anyhow::Result;
use chrono::{Local, NaiveDateTime};
use lf_ask::{Corpus, SearchOptions, Since};
use lf_core::{config::Config, history::HistoryStore, home::expand_user, symbols::pycompat as py};
use lf_meetings::{
    answer_question,
    dispatch::{self, PromptQueue, PromptTransitionError},
    format_transcript, generate_title, latest_transcript_log, read_transcript_log, templates,
    titles::{is_default_title, TITLE_TIMEOUT_SECONDS},
    MeetingSummarizer, MeetingWatcher, ObsidianWriter, Segment, WriteNote,
};
use lf_runtime::menubar::{MeetingStatusPoller, MenuState};
use lf_server::backend::{Backend, BackendRequest, BackendResponse, UnavailableBackend};
use serde_json::{json, Value};
use session::SessionActor;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard},
    time::Duration,
};
use validation::{Body, ResponseResult};

#[derive(Default, PartialEq)]
enum Phase {
    #[default]
    Idle,
    Starting,
    Running,
    Stopping,
}

fn remove_recovery(saved: lf_meetings::writer::SavedNote) {
    for path in [saved.notes_path, saved.log_path] {
        if let Err(error) = std::fs::remove_file(path) {
            eprintln!("could not remove completed meeting recovery: {error}");
        }
    }
}
#[derive(Clone, Default)]
struct Metadata {
    title: String,
    title_auto: bool,
    category: String,
    template: String,
    user_notes: String,
}
#[derive(Default)]
struct State {
    closed: bool,
    phase: Phase,
    session: Option<Arc<SessionActor>>,
    meta: Metadata,
    last_saved: Option<Value>,
}

pub struct MeetingsBackend {
    inner: Arc<dyn Backend>,
    capture: Arc<dyn CaptureFactory>,
    providers: Arc<dyn ProviderFactory>,
    queue: PromptQueue,
    history_path: Option<PathBuf>,
    watcher: Option<Arc<MeetingWatcher>>,
    transcribe_lock: Arc<Mutex<()>>,
    state: Mutex<State>,
    idle: Condvar,
}
impl MeetingsBackend {
    /// Poll the same in-process status as GET /meeting/status using live config snapshots.
    pub fn start_status_poller(
        self: &Arc<Self>,
        config: Arc<dyn Fn() -> Result<Config, String> + Send + Sync>,
        menu: MenuState,
    ) -> MeetingStatusPoller {
        let backend = self.clone();
        MeetingStatusPoller::start(
            Arc::new(move || config().map(|config| backend.status(&config))),
            Arc::new(move |result| menu.change(|model| model.apply_status(&result))),
        )
    }

    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: Arc::new(UnavailableBackend),
            capture: Arc::new(UnavailableCapture),
            providers: Arc::new(ConfiguredProviders),
            queue: PromptQueue::at_default_path()?,
            history_path: None,
            watcher: None,
            transcribe_lock: Arc::default(),
            state: Mutex::default(),
            idle: Condvar::new(),
        })
    }
    pub fn with_inner(mut self, inner: Arc<dyn Backend>) -> Self {
        self.inner = inner;
        self
    }
    pub fn with_capture(mut self, capture: Arc<dyn CaptureFactory>) -> Self {
        self.capture = capture;
        self
    }
    pub fn with_providers(mut self, providers: Arc<dyn ProviderFactory>) -> Self {
        self.providers = providers;
        self
    }
    pub fn with_queue(mut self, queue: PromptQueue) -> Self {
        self.queue = queue;
        self
    }
    pub fn with_history_path(mut self, path: PathBuf) -> Self {
        self.history_path = Some(path);
        self
    }
    pub fn with_watcher(mut self, watcher: Arc<MeetingWatcher>) -> Self {
        self.watcher = Some(watcher);
        self
    }
    pub fn with_transcribe_lock(mut self, lock: Arc<Mutex<()>>) -> Self {
        self.transcribe_lock = lock;
        self
    }
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn transcript_path(&self, config: &Config, last: Option<&Value>) -> Option<PathBuf> {
        if let Some(path) = last
            .and_then(|v| v["log_path"].as_str())
            .map(PathBuf::from)
            .filter(|p| p.is_file())
        {
            return Some(path);
        }
        latest_transcript_log(&expanded(config, "vault_path").join(text(config, "logs_folder")))
            .ok()
            .flatten()
    }
    fn status(&self, config: &Config) -> Value {
        let (session, meta, last) = {
            let state = self.state();
            (
                state.session.clone(),
                state.meta.clone(),
                state.last_saved.clone(),
            )
        };
        let live = session.and_then(|s| s.snapshot()).map(|s| {
            let segments: Vec<_> = s.segments.iter().skip(s.segments.len().saturating_sub(8)).map(|s| json!({"stamp":s.stamp(),"text":s.text,"source":s.source})).collect();
            json!({"title":meta.title,"title_auto":meta.title_auto,"category":meta.category,"template":meta.template,"user_notes":meta.user_notes,"system_audio":s.system_audio,"system_audio_error":s.system_audio_error,"seconds":s.seconds as u64,"segments":segments,"segment_count":s.segments.len()})
        });
        let watch = self
            .watcher
            .as_ref()
            .map(|w| w.status())
            .unwrap_or_default();
        let watch_error = if self.watcher.is_none()
            && enabled(config, "meetings_enabled")
            && enabled(config, "meeting_watch")
        {
            "meeting watcher unavailable"
        } else {
            &watch.error
        };
        json!({"watching":enabled(config,"meetings_enabled") && enabled(config,"meeting_watch") && watch_error.is_empty(),"watch_error":watch_error,"mic_busy":watch.mic_busy,"detected":watch.detected,"platform":watch.platform,"auto_start":enabled(config,"meeting_auto_start"),"capture_system_audio":enabled(config,"capture_system_audio"),"session":live,"last_saved":last,"chat_available":self.transcript_path(config,last.as_ref()).is_some()})
    }
    fn start(&self, request: &BackendRequest) -> ResponseResult<Value> {
        let config = &request.config;
        let mut body = Body::parse(request)?;
        let mut title = body.string("title", Some("Meeting"), Some(200));
        let mut category = body.string("category", Some(""), None);
        let template = body.string("template", Some(""), None);
        let user_notes = body.string("user_notes", Some(""), None);
        let auto = body.boolean("auto_title", None);
        body.finish()?;
        let title_auto = if py::strip(&title).is_empty() {
            title.clear();
            true
        } else {
            auto.unwrap_or_else(|| is_default_title(Some(&title)))
        };
        if !template.is_empty()
            && !templates::load_templates(Some(&expanded(config, "meeting_templates_dir")))
                .contains_key(&template)
        {
            return Err(BackendResponse::error(
                400,
                format!("unknown template {}", py::repr_str(&template)),
            ));
        }
        if user_notes.chars().count() > 100_000 {
            return Err(BackendResponse::error(413, "notes too long"));
        }
        {
            let mut state = self.state();
            if state.closed || state.phase != Phase::Idle {
                return Err(BackendResponse::error(
                    409,
                    "a meeting session is already running",
                ));
            }
            state.phase = Phase::Starting;
        }
        let mut reset = ResetSlot {
            backend: self,
            armed: true,
        };
        if category.is_empty() {
            category = self
                .watcher
                .as_ref()
                .map(|w| w.status().platform)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Other".into());
        }
        if let Some(watcher) = &self.watcher {
            watcher.pause();
        }
        let opened = SessionActor::open(
            config.clone(),
            self.capture.clone(),
            self.transcribe_lock.clone(),
        );
        let session = match opened {
            Ok(session) => Arc::new(session),
            Err(error) => {
                if let Some(watcher) = &self.watcher {
                    watcher.resume();
                }
                return Err(BackendResponse::error(
                    500,
                    format!("mic open failed: {error}"),
                ));
            }
        };
        let snapshot = session.snapshot().ok_or_else(internal)?;
        let template = if template.is_empty() {
            text(config, "meeting_default_template").into()
        } else {
            template
        };
        let response = json!({"ok":true,"category":category,"template":template,"system_audio":snapshot.system_audio,"system_audio_error":snapshot.system_audio_error});
        let mut state = self.state();
        if state.closed {
            drop(state);
            let _ = session.stop();
            return Err(BackendResponse::error(409, "meeting service is stopping"));
        }
        state.meta = Metadata {
            title: if title.is_empty() {
                "Meeting".into()
            } else {
                title
            },
            title_auto,
            category,
            template,
            user_notes,
        };
        state.session = Some(session);
        state.last_saved = None;
        state.phase = Phase::Running;
        reset.armed = false;
        Ok(response)
    }
    fn notes(&self, request: &BackendRequest) -> ResponseResult<Value> {
        let mut body = Body::parse(request)?;
        let notes = body.string("notes", None, None);
        body.finish()?;
        let chars = notes.chars().count();
        if chars > 100_000 {
            return Err(BackendResponse::error(413, "notes too long"));
        }
        let mut state = self.state();
        if state.phase != Phase::Running {
            return Err(BackendResponse::error(409, "no meeting session running"));
        }
        state.meta.user_notes = notes;
        Ok(json!({"ok":true,"chars":chars}))
    }
    fn stop(&self, config: &Config) -> ResponseResult<Value> {
        let (session, meta) = {
            let mut state = self.state();
            if state.phase != Phase::Running {
                return Err(BackendResponse::error(409, "no meeting session running"));
            }
            let session = state.session.take().ok_or_else(internal)?;
            state.phase = Phase::Stopping;
            (session, state.meta.clone())
        };
        let _reset = ResetSlot {
            backend: self,
            armed: true,
        };
        let writer = ObsidianWriter::new(
            Path::new(text(config, "vault_path")),
            Path::new(text(config, "notes_folder")),
            Path::new(text(config, "logs_folder")),
        );
        let mut recovery = session.snapshot().and_then(|snapshot| {
            self.checkpoint(
                &writer,
                &meta,
                snapshot.started_at,
                snapshot.seconds,
                &snapshot.segments,
            )
        });
        let finished = session.stop();
        if let Some(watcher) = &self.watcher {
            watcher.resume();
        }
        let finished = finished.map_err(|_| internal())?;
        if let Some(saved) = self.checkpoint(
            &writer,
            &meta,
            finished.started_at,
            finished.seconds,
            &finished.segments,
        ) {
            if let Some(previous) = recovery.replace(saved) {
                remove_recovery(previous);
            }
        }
        let transcript = format_transcript(&finished.segments);
        let template = templates::resolve_template(
            Some(&meta.template),
            Some(&expanded(config, "meeting_templates_dir")),
        );
        let llm = self
            .providers
            .llm(config)
            .unwrap_or_else(|error| Box::new(providers::FailedProvider(error.to_string())));
        let mut notes =
            MeetingSummarizer::new(&*llm).summarize(&transcript, &meta.user_notes, Some(&template));
        let (title, generated) = if meta.title_auto {
            generate_title(
                self.providers.title(config),
                &transcript,
                &meta.user_notes,
                finished.started_at,
                Duration::from_secs_f64(TITLE_TIMEOUT_SECONDS),
            )
        } else {
            (meta.title, false)
        };
        let mut extra = json!({});
        if enabled(config, "work_prompts") {
            if let Some(llm) = self.providers.work_prompts(config) {
                let prompts_md = workprompts::generate(&*llm, &notes);
                if !prompts_md.is_empty() {
                    notes.push_str(&format!("\n\n## Suggested Work Prompts\n\n{prompts_md}"));
                    let _ = (|| -> Result<()> {
                        let prompts = dispatch::parse_work_prompts(&prompts_md);
                        if !prompts.is_empty() {
                            let dir = expanded(config, "prompts_dir");
                            let paths = dispatch::write_prompts(
                                &prompts,
                                &title,
                                finished.started_at,
                                &dir,
                            )?;
                            self.queue.add(&prompts, &paths, &title)?;
                            extra = json!({"prompts":paths.len(),"prompts_dir":dir});
                        }
                        Ok(())
                    })();
                }
            }
        }
        let saved = writer
            .write(&WriteNote {
                title: &title,
                category: &meta.category,
                started_at: finished.started_at,
                duration_seconds: finished.seconds,
                notes_md: &notes,
                segments: &finished.segments,
                user_notes: &meta.user_notes,
                template: &template.id,
                generated_title: meta.title_auto,
            })
            .map_err(|_| internal())?;
        if let Some(recovery) = recovery {
            remove_recovery(recovery);
        }
        let mut payload = json!({"title":saved.title,"title_generated":generated,"notes_path":saved.notes_path,"log_path":saved.log_path,"segments":finished.segments.len(),"template":template.id,"enhanced":!py::strip(&meta.user_notes).is_empty()});
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        self.state().last_saved = Some(payload.clone());
        Ok(payload)
    }
    fn checkpoint(
        &self,
        writer: &ObsidianWriter,
        meta: &Metadata,
        started_at: NaiveDateTime,
        seconds: f64,
        segments: &[Segment],
    ) -> Option<lf_meetings::writer::SavedNote> {
        let result = writer.write(&WriteNote {
            title: &format!("{} (recovery)", meta.title),
            category: &meta.category,
            started_at,
            duration_seconds: seconds,
            notes_md: &meta.user_notes,
            segments,
            user_notes: &meta.user_notes,
            template: &meta.template,
            generated_title: false,
        });
        match result {
            Ok(saved) => {
                self.state().last_saved = Some(
                    json!({"title":saved.title,"notes_path":saved.notes_path,"log_path":saved.log_path,"recovery":true}),
                );
                Some(saved)
            }
            Err(error) => {
                eprintln!("could not checkpoint meeting: {error}");
                None
            }
        }
    }
    fn chat(&self, request: &BackendRequest) -> ResponseResult<Value> {
        let question = question(request, 2000)?;
        let last = self.state().last_saved.clone();
        let path = self
            .transcript_path(&request.config, last.as_ref())
            .ok_or_else(|| BackendResponse::error(404, "no saved meeting transcript yet"))?;
        let transcript = read_transcript_log(&path).map_err(|_| internal())?;
        let answer = (|| -> Result<String> {
            let llm = self.providers.llm(&request.config)?;
            Ok(answer_question(&*llm, &transcript, &question, 4000)?)
        })()
        .map_err(|e| BackendResponse::error(502, format!("LLM error: {e}")))?;
        Ok(json!({"answer":answer,"log_path":path}))
    }
    fn recall(&self, request: &BackendRequest) -> ResponseResult<Value> {
        let mut body = Body::parse(request)?;
        let question = body.string("question", None, None);
        let since = body.optional_string("since").map(Since::Text);
        let k = body.integer("k", 8);
        let llm = body.boolean("llm", Some(true)).unwrap_or(true);
        body.finish()?;
        let question = checked_question(&question, 1000)?;
        if !(1..=20).contains(&k) {
            return Err(BackendResponse::error(400, "k must be between 1 and 20"));
        }
        let now = Local::now().naive_local();
        lf_ask::parse_since(since.as_ref(), now)
            .map_err(|e| BackendResponse::error(400, e.to_string()))?;
        let options = SearchOptions {
            k: k as usize,
            since,
            now,
            ..Default::default()
        };
        let result = (|| -> Result<Value> {
            let store = if enabled(&request.config, "history_enabled") {
                Some(HistoryStore::open(self.history_path.clone(), None, true)?)
            } else {
                None
            };
            let corpus = Corpus::from_config(&request.config, store.as_ref());
            let result = if llm {
                let llm = self.providers.llm(&request.config)?;
                lf_ask::recall(&question, &*llm, &corpus, &options)?
            } else {
                lf_ask::search(&question, &corpus, &options)?
            };
            Ok(result.to_dict())
        })();
        result.map_err(|e| BackendResponse::error(500, format!("recall failed: {e}")))
    }
}
impl MeetingsBackend {
    fn finish_shutdown_stop(&self, result: ResponseResult<Value>) -> Result<()> {
        if let Err(error) = result {
            anyhow::ensure!(
                error.status == 409,
                "could not save meeting during shutdown"
            );
            let state = self.state();
            let _idle = self
                .idle
                .wait_while(state, |state| state.phase != Phase::Idle)
                .unwrap_or_else(|error| error.into_inner());
        }
        Ok(())
    }
}

impl Backend for MeetingsBackend {
    fn shutdown(&self, config: &Config) -> Result<()> {
        self.state().closed = true;
        if let Some(watcher) = &self.watcher {
            watcher.stop();
        }
        self.finish_shutdown_stop(self.stop(config))?;
        self.inner.shutdown(config)
    }

    fn handle(&self, request: BackendRequest) -> BackendResponse {
        let result = match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/meeting/templates") => {
                let templates = templates::load_templates(Some(&expanded(
                    &request.config,
                    "meeting_templates_dir",
                )));
                let default = text(&request.config, "meeting_default_template");
                let default = if templates.contains_key(default) {
                    default
                } else {
                    templates::DEFAULT_TEMPLATE_ID
                };
                let mut order: Vec<_> = templates::load_templates(None).into_keys().collect();
                let mut paths: Vec<_> =
                    std::fs::read_dir(expanded(&request.config, "meeting_templates_dir"))
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|e| e.path())
                        .collect();
                if cfg!(windows) {
                    paths.sort_by_key(|p| p.to_string_lossy().to_lowercase());
                } else {
                    paths.sort();
                }
                for path in paths {
                    let extension = path.extension().unwrap_or_default().to_string_lossy();
                    if !(extension == "md" || cfg!(windows) && extension.eq_ignore_ascii_case("md"))
                    {
                        continue;
                    }
                    let id = path
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_lowercase();
                    if templates.contains_key(&id) && !order.contains(&id) {
                        order.push(id);
                    }
                }
                Ok(
                    json!({"default":default,"templates":order.iter().filter_map(|id| templates.get(id)).map(|t| json!({"id":t.id,"name":t.name,"description":t.description,"sections":t.section_names()})).collect::<Vec<_>>()}),
                )
            }
            ("GET", "/meeting/status") => Ok(self.status(&request.config)),
            ("POST", "/meeting/start") => self.start(&request),
            ("POST", "/meeting/notes") => self.notes(&request),
            ("POST", "/meeting/chat") => self.chat(&request),
            ("POST", "/meeting/stop") => self.stop(&request.config),
            ("POST", "/meeting/dismiss") => {
                if let Some(watcher) = &self.watcher {
                    watcher.dismiss();
                }
                Ok(json!({"ok":true}))
            }
            ("POST", "/api/recall") => self.recall(&request),
            ("GET", "/prompts") => self
                .queue
                .list(request.query.get("status").map(String::as_str))
                .map(|entries| json!({"entries":entries}))
                .map_err(|_| internal()),
            ("POST", path) if prompt_action(path).is_some() => {
                let (id, verb) = prompt_action(path).unwrap();
                let result = if verb == "approve" {
                    dispatch::approve_prompt(&self.queue, id)
                } else {
                    dispatch::reject_prompt(&self.queue, id)
                };
                result.map_err(|e| {
                    if let Some(error) = e.downcast_ref::<PromptTransitionError>() {
                        BackendResponse::error(error.status_code, &error.detail)
                    } else {
                        internal()
                    }
                })
            }
            _ => return self.inner.handle(request),
        };
        match result {
            Ok(body) => BackendResponse { status: 200, body },
            Err(response) => response,
        }
    }
    fn stt_available(&self, provider: &str) -> bool {
        self.inner.stt_available(provider)
    }
    fn transcriber_loaded(&self) -> bool {
        self.inner.transcriber_loaded()
    }
    fn autostart_status(&self) -> String {
        self.inner.autostart_status()
    }
}

struct ResetSlot<'a> {
    backend: &'a MeetingsBackend,
    armed: bool,
}
impl Drop for ResetSlot<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.backend.state().phase = Phase::Idle;
            self.backend.idle.notify_all();
        }
    }
}
fn internal() -> BackendResponse {
    BackendResponse::error(500, "Internal Server Error")
}
fn text<'a>(config: &'a Config, key: &str) -> &'a str {
    config.field_str(key).unwrap_or("")
}
fn enabled(config: &Config, key: &str) -> bool {
    config.field_bool(key) == Some(true)
}
fn expanded(config: &Config, key: &str) -> PathBuf {
    expand_user(Path::new(text(config, key))).unwrap_or_else(|_| PathBuf::from(text(config, key)))
}
fn prompt_action(path: &str) -> Option<(&str, &str)> {
    let (id, verb) = path.strip_prefix("/prompts/")?.split_once('/')?;
    (!id.is_empty() && matches!(verb, "approve" | "reject")).then_some((id, verb))
}
fn checked_question(question: &str, max: usize) -> ResponseResult<String> {
    let question = py::strip(question);
    if question.is_empty() {
        return Err(BackendResponse::error(400, "question is empty"));
    }
    if question.chars().count() > max {
        return Err(BackendResponse::error(413, "question too long"));
    }
    Ok(question.into())
}
fn question(request: &BackendRequest, max: usize) -> ResponseResult<String> {
    let mut body = Body::parse(request)?;
    let question = body.string("question", None, None);
    body.finish()?;
    checked_question(&question, max)
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn conflict_after_shutdown_stop_claim_waits_for_persistence() {
        let backend = Arc::new(MeetingsBackend::new().unwrap());
        backend.state().phase = Phase::Stopping;
        let (entered, started) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let closing = backend.clone();
        let shutdown = std::thread::spawn(move || {
            entered.send(()).unwrap();
            finished
                .send(
                    closing
                        .finish_shutdown_stop(Err(BackendResponse::error(
                            409,
                            "stop raced shutdown",
                        )))
                        .is_ok(),
                )
                .unwrap();
        });
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(done.recv_timeout(Duration::from_millis(20)).is_err());
        backend.state().phase = Phase::Idle;
        backend.idle.notify_all();
        assert!(done.recv_timeout(Duration::from_secs(5)).unwrap());
        shutdown.join().unwrap();
        assert!(backend
            .finish_shutdown_stop(Err(BackendResponse::error(500, "disk error")))
            .is_err());
        assert!(backend
            .finish_shutdown_stop(Err(BackendResponse::error(409, "already idle")))
            .is_ok());
    }
}
