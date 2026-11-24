use anyhow::Result;
use axum::{
    body::{to_bytes, Body},
    http::Request,
    Router,
};
use lf_core::{
    config::Config,
    history::{HistoryStore, NewEntry},
};
use lf_meetings::{
    dispatch::{PromptQueue, WorkPrompt},
    AudioCallback, AudioSource, SystemAudioUnavailable, Transcriber,
};
use lf_meetings_backend::{CaptureFactory, MeetingsBackend, ProviderFactory, TitleLlm};
use lf_providers::{Health, LlmProvider, ModelInfo, ProviderError};
use lf_server::{
    backend::{Backend, BackendRequest, BackendResponse},
    router, ServerState,
};
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};
use tempfile::TempDir;
use tower::ServiceExt;

type Gate = (mpsc::Sender<()>, mpsc::Receiver<()>);
type Call = (String, String, String, Option<i64>);
#[derive(Default)]
struct Capture {
    fail: AtomicBool,
    system: AtomicBool,
    opened: AtomicUsize,
    stt_fail: AtomicBool,
    started: Arc<AtomicUsize>,
    stopped: Arc<AtomicUsize>,
    gate: Mutex<Option<Gate>>,
    transcription_gate: Arc<Mutex<Option<Gate>>>,
}
struct Device {
    _local: Rc<()>,
    owner: std::thread::ThreadId,
    sample: f32,
    started: Arc<AtomicUsize>,
    stopped: Arc<AtomicUsize>,
}
impl AudioSource for Device {
    fn start(
        &mut self,
        callback: AudioCallback,
    ) -> std::result::Result<(), SystemAudioUnavailable> {
        assert_eq!(self.owner, std::thread::current().id());
        self.started.fetch_add(1, Ordering::SeqCst);
        callback(vec![self.sample; 16000]);
        Ok(())
    }
    fn stop(&mut self) -> std::result::Result<(), SystemAudioUnavailable> {
        assert_eq!(self.owner, std::thread::current().id());
        self.stopped.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn label(&self) -> &str {
        "synthetic"
    }
}
struct Stt {
    gate: Arc<Mutex<Option<Gate>>>,
}
impl Transcriber for Stt {
    fn transcribe(&self, audio: &[f32], _: Option<&str>) -> Result<String> {
        if let Some((entered, release)) = self.gate.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        Ok(if audio[0] < 0.15 {
            "we will launch on March 3"
        } else {
            "sounds good, Dana owns the blog post"
        }
        .into())
    }
}
impl Capture {
    fn device(&self, sample: f32) -> Box<dyn AudioSource> {
        Box::new(Device {
            _local: Rc::new(()),
            owner: std::thread::current().id(),
            sample,
            started: self.started.clone(),
            stopped: self.stopped.clone(),
        })
    }
}
impl CaptureFactory for Capture {
    fn transcriber(&self, _: &Config) -> Result<Arc<dyn Transcriber>> {
        if let Some((entered, release)) = self.gate.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        anyhow::ensure!(
            !self.stt_fail.load(Ordering::SeqCst),
            "model not downloaded"
        );
        Ok(Arc::new(Stt {
            gate: self.transcription_gate.clone(),
        }))
    }
    fn microphone(&self, _: &Config) -> Result<Box<dyn AudioSource>> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(!self.fail.load(Ordering::SeqCst), "permission denied");
        Ok(self.device(0.1))
    }
    fn system_audio(&self, _: &Config) -> Result<Option<Box<dyn AudioSource>>> {
        anyhow::ensure!(self.system.load(Ordering::SeqCst), "install loopback");
        Ok(Some(self.device(0.2)))
    }
}
#[derive(Default)]
struct Models {
    calls: Mutex<Vec<Call>>,
    fail: AtomicBool,
    work: AtomicBool,
    title_fail: AtomicBool,
    gate: Mutex<Option<Gate>>,
}
struct Llm {
    models: Arc<Models>,
    model: String,
    kind: &'static str,
}
impl LlmProvider for Llm {
    fn id(&self) -> &'static str {
        "mock"
    }
    fn offline(&self) -> bool {
        true
    }
    fn complete(
        &self,
        system: &str,
        user: &str,
        max: Option<i64>,
    ) -> std::result::Result<String, ProviderError> {
        self.models.calls.lock().unwrap().push((
            self.model.clone(),
            system.into(),
            user.into(),
            max,
        ));
        if self.kind == "main" {
            if let Some((entered, release)) = self.models.gate.lock().unwrap().take() {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(10)).unwrap();
            }
        }
        if self.models.fail.load(Ordering::SeqCst)
            || self.kind == "title" && self.models.title_fail.load(Ordering::SeqCst)
        {
            return Err(ProviderError("ollama not running".into()));
        }
        Ok(match self.kind {
            "title" => "Pricing Rollout",
            "work" => "### Ship launch tests\n> Implement the pricing tests before launch.",
            _ if user.contains("Question:") => "March 3 [1] [00:00].",
            _ => "## Launch\n- March 3\n\n## Summary\nnotes",
        }
        .into())
    }
    fn list_models(&self) -> std::result::Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![])
    }
    fn healthcheck(&self) -> Health {
        Health::ok()
    }
}
struct Providers(Arc<Models>);
impl Providers {
    fn model(&self, config: &Config, kind: &'static str) -> Llm {
        Llm {
            models: self.0.clone(),
            model: config.field_str("llm_model").unwrap().into(),
            kind,
        }
    }
}
impl ProviderFactory for Providers {
    fn llm(&self, config: &Config) -> Result<Box<dyn LlmProvider>> {
        Ok(Box::new(self.model(config, "main")))
    }
    fn title(&self, config: &Config) -> Option<TitleLlm> {
        Some(Arc::new(self.model(config, "title")))
    }
    fn work_prompts(&self, config: &Config) -> Option<Box<dyn LlmProvider>> {
        self.0
            .work
            .load(Ordering::SeqCst)
            .then(|| Box::new(self.model(config, "work")) as Box<dyn LlmProvider>)
    }
}
struct Fixture {
    dir: TempDir,
    config: Config,
    capture: Arc<Capture>,
    models: Arc<Models>,
    backend: Arc<MeetingsBackend>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("templates")).unwrap();
        fs::write(
            dir.path().join("templates/retro.md"),
            "---\nname: Retro\n---\n## Went Well\nBullets.\n",
        )
        .unwrap();
        let mut config = Config::default();
        for (key, value) in [
            ("meetings_enabled", json!(false)),
            ("meeting_watch", json!(false)),
            ("work_prompts", json!(false)),
            ("history_enabled", json!(false)),
            ("sounds_enabled", json!(false)),
            ("vault_path", json!(dir.path().join("vault"))),
            ("meeting_templates_dir", json!(dir.path().join("templates"))),
            ("prompts_dir", json!(dir.path().join("prompts"))),
        ] {
            config.set_field(key, value);
        }
        let capture = Arc::new(Capture::default());
        let models = Arc::new(Models::default());
        let backend = Self::backend(dir.path(), capture.clone(), models.clone());
        Self {
            dir,
            config,
            capture,
            models,
            backend,
        }
    }
    fn backend(dir: &Path, capture: Arc<Capture>, models: Arc<Models>) -> Arc<MeetingsBackend> {
        Arc::new(
            MeetingsBackend::new()
                .unwrap()
                .with_capture(capture)
                .with_providers(Arc::new(Providers(models)))
                .with_queue(PromptQueue::new(dir.join("queue.json")))
                .with_history_path(dir.join("history.sqlite")),
        )
    }
    fn app(&self) -> Router {
        app(self.config.clone(), self.backend.clone())
    }
}
fn app(config: Config, backend: Arc<dyn Backend>) -> Router {
    router(Arc::new(ServerState::new(
        config,
        "test-token".into(),
        backend,
    )))
}
async fn request(app: Router, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    raw(
        app,
        method,
        path,
        body.map(|v| v.to_string()).unwrap_or_default(),
        true,
        "localhost",
    )
    .await
}
async fn raw(
    app: Router,
    method: &str,
    path: &str,
    body: String,
    token: bool,
    host: &str,
) -> (u16, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("Host", host)
        .header("Content-Type", "application/json");
    if token {
        builder = builder.header("X-Localflow-Token", "test-token");
    }
    let response = app
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}
fn gate(slot: &Mutex<Option<Gate>>) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *slot.lock().unwrap() = Some((entered_tx, release_rx));
    (entered_rx, release_tx)
}

#[tokio::test]
async fn templates_defaults_and_unknown_template_do_not_claim_slot() {
    let mut f = Fixture::new();
    let data = request(f.app(), "GET", "/meeting/templates", None).await;
    assert_eq!(data.0, 200);
    assert_eq!(data.1["default"], "general");
    assert_eq!(
        data.1["templates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["general", "one-on-one", "standup", "retro"]
    );
    assert_eq!(data.1["templates"][1]["name"], "1:1");
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/meeting/start",
            Some(json!({"template":"nope"}))
        )
        .await,
        (400, json!({"detail":"unknown template 'nope'"}))
    );
    assert_eq!(f.capture.opened.load(Ordering::SeqCst), 0);
    f.config
        .set_field("meeting_default_template", json!("retro"));
    assert_eq!(
        request(f.app(), "POST", "/meeting/start", Some(json!({})))
            .await
            .1["template"],
        "retro"
    );
    assert_eq!(
        request(f.app(), "POST", "/meeting/stop", None).await.1["template"],
        "retro"
    );
    f.config
        .set_field("meeting_default_template", json!("gone"));
    assert_eq!(
        request(f.app(), "GET", "/meeting/templates", None).await.1["default"],
        "general"
    );
}

#[tokio::test]
async fn lifecycle_notes_speakers_files_chat_restart_and_errors() {
    let mut f = Fixture::new();
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/meeting/notes",
            Some(json!({"notes":"x"}))
        )
        .await,
        (409, json!({"detail":"no meeting session running"}))
    );
    assert_eq!(request(f.app(), "POST", "/meeting/stop", None).await.0, 409);
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/meeting/chat",
            Some(json!({"question":"q"}))
        )
        .await
        .0,
        404
    );
    assert_eq!(
        request(f.app(), "GET", "/meeting/status", None).await.1,
        json!({"watching":false,"watch_error":"","mic_busy":false,"detected":false,"platform":"","auto_start":true,"capture_system_audio":false,"session":null,"last_saved":null,"chat_available":false})
    );
    f.config.set_field("capture_system_audio", json!(true));
    f.capture.system.store(true, Ordering::SeqCst);
    let start = request(
        f.app(),
        "POST",
        "/meeting/start",
        Some(json!({"title":"Launch sync","template":"standup","user_notes":"- launch"})),
    )
    .await;
    assert_eq!(
        start,
        (
            200,
            json!({"ok":true,"category":"Other","template":"standup","system_audio":"on","system_audio_error":""})
        )
    );
    let notes = "## Launch\n- **date?**\n- blog";
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/meeting/notes",
            Some(json!({"notes":notes}))
        )
        .await,
        (200, json!({"ok":true,"chars":notes.chars().count()}))
    );
    let live = request(f.app(), "GET", "/meeting/status", None).await.1;
    assert_eq!(live["session"]["title"], "Launch sync");
    assert_eq!(live["session"]["user_notes"], notes);
    assert_eq!(live["session"]["title_auto"], false);
    let saved = request(f.app(), "POST", "/meeting/stop", None).await;
    assert_eq!(saved.0, 200, "{}", saved.1);
    assert_eq!(saved.1["title"], "Launch sync");
    assert_eq!(saved.1["title_generated"], false);
    assert_eq!(saved.1["segments"], 2);
    assert_eq!(saved.1["template"], "standup");
    assert_eq!(saved.1["enhanced"], true);
    let log = fs::read_to_string(saved.1["log_path"].as_str().unwrap()).unwrap();
    assert!(log.contains("**Others:**") && log.contains("## My raw notes"));
    {
        let calls = f.models.calls.lock().unwrap();
        assert!(calls[0].2.contains(lf_meetings::ENHANCE_INSTRUCTION));
        assert!(calls[0].2.contains("## Yesterday") && calls[0].2.contains("Others: sounds good"));
    }
    let status = request(f.app(), "GET", "/meeting/status", None).await.1;
    assert!(status["session"].is_null());
    assert_eq!(status["last_saved"], saved.1);
    assert_eq!(status["chat_available"], true);
    let chat = request(
        f.app(),
        "POST",
        "/meeting/chat",
        Some(json!({"question":"When do we launch?"})),
    )
    .await;
    assert_eq!(
        chat,
        (
            200,
            json!({"answer":"March 3 [1] [00:00].","log_path":saved.1["log_path"]})
        )
    );
    assert!(!f
        .models
        .calls
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .2
        .contains("duration_minutes"));
    let restarted = Fixture::backend(f.dir.path(), f.capture.clone(), f.models.clone());
    assert_eq!(
        request(
            app(f.config.clone(), restarted),
            "POST",
            "/meeting/chat",
            Some(json!({"question":"q"}))
        )
        .await
        .1["log_path"],
        saved.1["log_path"]
    );
    f.models.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/meeting/chat",
            Some(json!({"question":"q"}))
        )
        .await,
        (502, json!({"detail":"LLM error: ollama not running"}))
    );
    assert_eq!(f.capture.stopped.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn server_shutdown_saves_active_meeting_and_is_idle_safe() {
    let f = Fixture::new();
    let state = ServerState::new(f.config.clone(), "test-token".into(), f.backend.clone());
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/meeting/start",
            Some(json!({"title":"Quit save", "auto_title":false}))
        )
        .await
        .0,
        200
    );
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/meeting/notes",
            Some(json!({"notes":"Unsaved decision"}))
        )
        .await
        .0,
        200
    );
    state.shutdown().unwrap();
    let status = request(f.app(), "GET", "/meeting/status", None).await.1;
    assert!(status["session"].is_null());
    let saved = &status["last_saved"];
    let log = fs::read_to_string(saved["log_path"].as_str().unwrap()).unwrap();
    assert!(log.contains("Unsaved decision"));
    assert!(log.contains("we will launch on March 3"));
    assert!(Path::new(saved["notes_path"].as_str().unwrap()).is_file());
    assert_eq!(f.capture.stopped.load(Ordering::SeqCst), 1);
    state.shutdown().unwrap();
    assert_eq!(
        request(f.app(), "POST", "/meeting/start", Some(json!({})))
            .await
            .0,
        409
    );
}

#[tokio::test]
async fn raw_notes_and_transcript_are_durable_before_slow_finalization() {
    let f = Fixture::new();
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/meeting/start",
            Some(
                json!({"title":"Recovery", "auto_title":false,"user_notes":"do not lose this note"})
            )
        )
        .await
        .0,
        200
    );
    let (transcribing, transcribed) = mpsc::channel();
    let (finish_transcription, transcription) = mpsc::channel();
    *f.capture.transcription_gate.lock().unwrap() = Some((transcribing, transcription));
    let (summarizing, summarized) = mpsc::channel();
    let (finish_summary, summary) = mpsc::channel();
    *f.models.gate.lock().unwrap() = Some((summarizing, summary));
    let backend = f.backend.clone();
    let config = f.config.clone();
    let stop = std::thread::spawn(move || {
        backend.handle(BackendRequest {
            method: "POST".parse().unwrap(),
            path: "/meeting/stop".into(),
            body: Default::default(),
            headers: Default::default(),
            query: Default::default(),
            config,
        })
    });
    transcribed.recv_timeout(Duration::from_secs(5)).unwrap();
    let before = request(f.app(), "GET", "/meeting/status", None).await.1["last_saved"].clone();
    assert_eq!(before["recovery"], true);
    assert!(fs::read_to_string(before["log_path"].as_str().unwrap())
        .unwrap()
        .contains("do not lose this note"));
    finish_transcription.send(()).unwrap();
    summarized.recv_timeout(Duration::from_secs(5)).unwrap();
    let during = request(f.app(), "GET", "/meeting/status", None).await.1["last_saved"].clone();
    let raw = fs::read_to_string(during["log_path"].as_str().unwrap()).unwrap();
    assert!(raw.contains("we will launch on March 3"));
    assert!(raw.contains("do not lose this note"));
    assert!(!Path::new(before["log_path"].as_str().unwrap()).exists());
    finish_summary.send(()).unwrap();
    let saved = stop.join().unwrap();
    assert_eq!(saved.status, 200);
    assert!(!Path::new(during["log_path"].as_str().unwrap()).exists());
    assert!(!Path::new(during["notes_path"].as_str().unwrap()).exists());
    assert!(Path::new(saved.body["log_path"].as_str().unwrap()).is_file());
}

#[tokio::test]
async fn server_shutdown_waits_for_an_already_stopping_meeting() {
    let f = Fixture::new();
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/meeting/start",
            Some(json!({"title":"Drain", "auto_title":false}))
        )
        .await
        .0,
        200
    );
    let (entered, summarizing) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *f.models.gate.lock().unwrap() = Some((entered, released));
    let backend = f.backend.clone();
    let config = f.config.clone();
    let stop = std::thread::spawn(move || {
        backend.handle(BackendRequest {
            method: "POST".parse().unwrap(),
            path: "/meeting/stop".into(),
            body: Default::default(),
            headers: Default::default(),
            query: Default::default(),
            config,
        })
    });
    summarizing.recv_timeout(Duration::from_secs(5)).unwrap();
    let state = ServerState::new(f.config.clone(), "test-token".into(), f.backend.clone());
    let (finished, done) = mpsc::channel();
    let shutdown = std::thread::spawn(move || finished.send(state.shutdown().is_ok()).unwrap());
    assert!(done.recv_timeout(Duration::from_millis(50)).is_err());
    release.send(()).unwrap();
    assert!(done.recv_timeout(Duration::from_secs(5)).unwrap());
    let saved = stop.join().unwrap();
    assert_eq!(saved.status, 200);
    assert!(Path::new(saved.body["notes_path"].as_str().unwrap()).is_file());
    shutdown.join().unwrap();
}

#[tokio::test]
async fn automatic_titles_fallback_explicit_title_and_current_snapshot() {
    let mut f = Fixture::new();
    for (body, expected, generated) in [
        (json!({}), "Pricing Rollout", true),
        (
            json!({"title":"Meeting","auto_title":false}),
            "Meeting",
            false,
        ),
        (
            json!({"title":"  ","auto_title":false}),
            "Pricing Rollout",
            true,
        ),
    ] {
        assert_eq!(
            request(f.app(), "POST", "/meeting/start", Some(body))
                .await
                .0,
            200
        );
        f.config.set_field("llm_model", json!("new-model"));
        let saved = request(f.app(), "POST", "/meeting/stop", None).await;
        assert_eq!(saved.0, 200, "{}", saved.1);
        assert_eq!(saved.1["title"], expected);
        assert_eq!(saved.1["title_generated"], generated);
    }
    assert!(f
        .models
        .calls
        .lock()
        .unwrap()
        .iter()
        .all(|c| c.0 == "new-model"));
    f.models.title_fail.store(true, Ordering::SeqCst);
    assert_eq!(
        request(f.app(), "POST", "/meeting/start", Some(json!({})))
            .await
            .0,
        200
    );
    let saved = request(f.app(), "POST", "/meeting/stop", None).await;
    assert!(saved.1["title"].as_str().unwrap().starts_with("Meeting "));
    assert_eq!(saved.1["title_generated"], false);
}

#[tokio::test]
async fn unavailable_capture_and_system_fallback_release_the_slot() {
    let mut f = Fixture::new();
    f.capture.fail.store(true, Ordering::SeqCst);
    for _ in 0..2 {
        assert_eq!(
            request(f.app(), "POST", "/meeting/start", Some(json!({}))).await,
            (500, json!({"detail":"mic open failed: permission denied"}))
        );
    }
    f.capture.fail.store(false, Ordering::SeqCst);
    f.config.set_field("capture_system_audio", json!(true));
    let started = request(f.app(), "POST", "/meeting/start", Some(json!({}))).await;
    assert_eq!(started.1["system_audio"], "error");
    assert_eq!(started.1["system_audio_error"], "install loopback");
    assert_eq!(request(f.app(), "POST", "/meeting/stop", None).await.0, 200);
    let backend = MeetingsBackend::new()
        .unwrap()
        .with_queue(PromptQueue::new(f.dir.path().join("other.json")));
    assert_eq!(
        request(
            app(f.config.clone(), Arc::new(backend)),
            "POST",
            "/meeting/start",
            Some(json!({}))
        )
        .await,
        (
            500,
            json!({"detail":"mic open failed: microphone unavailable"})
        )
    );
}

#[tokio::test]
async fn failed_transcriber_aborts_start_before_any_capture_and_releases_the_slot() {
    let mut f = Fixture::new();
    f.config.set_field("capture_system_audio", json!(true));
    f.capture.system.store(true, Ordering::SeqCst);
    f.capture.stt_fail.store(true, Ordering::SeqCst);
    assert_eq!(
        request(f.app(), "POST", "/meeting/start", Some(json!({}))).await,
        (
            500,
            json!({"detail":"mic open failed: model not downloaded"})
        )
    );
    assert_eq!(f.capture.opened.load(Ordering::SeqCst), 1);
    assert_eq!(f.capture.started.load(Ordering::SeqCst), 0);
    assert!(request(f.app(), "GET", "/meeting/status", None).await.1["session"].is_null());
    assert_eq!(request(f.app(), "POST", "/meeting/stop", None).await.0, 409);
    f.capture.stt_fail.store(false, Ordering::SeqCst);
    assert_eq!(
        request(f.app(), "POST", "/meeting/start", Some(json!({})))
            .await
            .0,
        200
    );
    assert_eq!(f.capture.started.load(Ordering::SeqCst), 2);
    assert_eq!(request(f.app(), "POST", "/meeting/stop", None).await.0, 200);
}

#[tokio::test]
async fn validation_limits_types_and_empty_inputs() {
    let f = Fixture::new();
    for (path, body, status) in [
        ("/meeting/start", json!({"title":"x".repeat(201)}), 422),
        ("/meeting/start", json!({"title":1,"category":false}), 422),
        ("/meeting/start", json!({"auto_title":"invalid"}), 422),
        (
            "/meeting/start",
            json!({"user_notes":"x".repeat(100001)}),
            413,
        ),
        ("/meeting/notes", json!({"notes":"x".repeat(100001)}), 413),
        ("/meeting/notes", json!({}), 422),
        ("/meeting/notes", json!({"notes":null}), 422),
        ("/meeting/chat", json!({"question":"  "}), 400),
        ("/meeting/chat", json!({"question":"x".repeat(2001)}), 413),
        ("/meeting/chat", json!({}), 422),
        ("/api/recall", json!({"question":" "}), 400),
        ("/api/recall", json!({"question":"x".repeat(1001)}), 413),
        ("/api/recall", json!({"question":"q","k":0}), 400),
        ("/api/recall", json!({"question":"q","k":21}), 400),
        (
            "/api/recall",
            json!({"question":"q","k":18446744073709551615_u64}),
            400,
        ),
        ("/api/recall", json!({"question":"q","k":1.5}), 422),
        ("/api/recall", json!({"question":"q","k":"abc"}), 422),
        ("/api/recall", json!({"question":"q","since":"soon"}), 400),
        ("/api/recall", json!({"question":"q","since":3}), 422),
        ("/api/recall", json!({"question":"q","llm":{}}), 422),
        ("/api/recall", json!([]), 422),
    ] {
        let response = request(f.app(), "POST", path, Some(body)).await;
        assert_eq!(response.0, status, "{path}: {}", response.1);
        assert!(response.1.get("detail").is_some());
    }
    assert_eq!(
        request(f.app(), "POST", "/meeting/notes", Some(json!({})))
            .await
            .1,
        json!({"detail":[{"type":"missing","loc":["body","notes"],"msg":"Field required","input":{}}]})
    );
    assert_eq!(
        request(f.app(), "POST", "/meeting/start", None).await.0,
        422
    );
    assert_eq!(
        request(f.app(), "POST", "/meeting/start", Some(Value::Null))
            .await
            .0,
        422
    );
    assert_eq!(
        raw(
            f.app(),
            "POST",
            "/meeting/start",
            "{".into(),
            true,
            "localhost"
        )
        .await
        .0,
        422
    );
    assert_eq!(f.capture.opened.load(Ordering::SeqCst), 0);
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/meeting/start",
            Some(json!({"title":"🦀".repeat(200),"auto_title":"false"}))
        )
        .await
        .0,
        200
    );
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/meeting/notes",
            Some(json!({"notes":"🦀".repeat(100000)}))
        )
        .await
        .1["chars"],
        100000
    );
}

#[tokio::test]
async fn coercions_and_media_types_follow_pydantic() {
    let f = Fixture::new();
    for k in [
        json!("8.0"),
        json!("1_0"),
        json!(" +8 "),
        json!(true),
        json!(8.0),
    ] {
        assert_eq!(
            request(
                f.app(),
                "POST",
                "/api/recall",
                Some(json!({"question":"q","k":k,"llm":"off"}))
            )
            .await,
            (200, json!({"answer":null,"citations":[]}))
        );
    }
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/api/recall",
            Some(json!({"question":"q","k":"9223372036854775808"}))
        )
        .await
        .0,
        400
    );
    for k in ["8.", "8e0", "1__0", "1.0_0", "١", "8\u{1c}"] {
        assert_eq!(
            request(
                f.app(),
                "POST",
                "/api/recall",
                Some(json!({"question":"q","k":k}))
            )
            .await
            .0,
            422
        );
    }
    let plain = Request::builder()
        .method("POST")
        .uri("/meeting/start")
        .header("Host", "localhost")
        .header("X-Localflow-Token", "test-token")
        .header("Content-Type", "text/plain")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(f.app().oneshot(plain).await.unwrap().status(), 422);
    for mime in [None, Some("application/vnd.localflow+json; charset=utf-8")] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/meeting/start")
            .header("Host", "localhost")
            .header("X-Localflow-Token", "test-token");
        if let Some(mime) = mime {
            request = request.header("Content-Type", mime);
        }
        assert_eq!(
            f.app()
                .oneshot(request.body(Body::from("{}")).unwrap())
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            self::request(f.app(), "POST", "/meeting/stop", None)
                .await
                .0,
            200
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn starting_and_stopping_are_exclusive_without_blocking_status() {
    let f = Fixture::new();
    let (entered, release) = gate(&f.capture.gate);
    let first = tokio::spawn(request(f.app(), "POST", "/meeting/start", Some(json!({}))));
    entered.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(
        request(f.app(), "POST", "/meeting/start", Some(json!({})))
            .await
            .0,
        409
    );
    assert_eq!(request(f.app(), "POST", "/meeting/stop", None).await.0, 409);
    assert!(request(f.app(), "GET", "/meeting/status", None).await.1["session"].is_null());
    release.send(()).unwrap();
    assert_eq!(first.await.unwrap().0, 200);
    let (entered, release) = gate(&f.models.gate);
    let stop = tokio::spawn(request(f.app(), "POST", "/meeting/stop", None));
    entered.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(
        request(f.app(), "POST", "/meeting/start", Some(json!({})))
            .await
            .0,
        409
    );
    assert_eq!(request(f.app(), "POST", "/meeting/stop", None).await.0, 409);
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/meeting/notes",
            Some(json!({"notes":"x"}))
        )
        .await
        .0,
        409
    );
    assert!(request(f.app(), "GET", "/meeting/status", None).await.1["session"].is_null());
    release.send(()).unwrap();
    assert_eq!(stop.await.unwrap().0, 200);
    assert_eq!(
        request(f.app(), "POST", "/meeting/start", Some(json!({})))
            .await
            .0,
        200
    );
}

#[tokio::test]
async fn recall_reads_history_and_meetings_with_citations_and_no_llm() {
    let mut f = Fixture::new();
    f.config.set_field("history_enabled", json!(true));
    let store = HistoryStore::open(Some(f.dir.path().join("history.sqlite")), None, true).unwrap();
    let mut row = NewEntry::new("dictate");
    row.final_text = "launch budget is 9000 approved by Dana".into();
    row.raw_text = row.final_text.clone();
    row.app_name = Some("Slack".into());
    store.add(&row).unwrap();
    let response = request(
        f.app(),
        "POST",
        "/api/recall",
        Some(json!({"question":"launch budget","llm":false,"since":"7d","k":"20"})),
    )
    .await;
    assert_eq!(response.0, 200, "{}", response.1);
    assert!(response.1["answer"].is_null());
    assert_eq!(response.1["citations"][0]["app"], "Slack");
    assert!(f.models.calls.lock().unwrap().is_empty());
    let llm = request(
        f.app(),
        "POST",
        "/api/recall",
        Some(json!({"question":"launch budget"})),
    )
    .await;
    assert_eq!(llm.0, 200);
    assert!(llm.1["answer"].as_str().unwrap().contains("March 3 [1]"));
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/api/recall",
            Some(json!({"question":"zebra quasar"}))
        )
        .await
        .1,
        json!({"answer":lf_ask::NO_ANSWER,"citations":[]})
    );
    f.models.fail.store(true, Ordering::SeqCst);
    assert!(request(
        f.app(),
        "POST",
        "/api/recall",
        Some(json!({"question":"budget"}))
    )
    .await
    .1["answer"]
        .is_null());
    f.config.set_field("history_enabled", json!(false));
    assert!(request(
        f.app(),
        "POST",
        "/api/recall",
        Some(json!({"question":"budget","llm":false}))
    )
    .await
    .1["citations"]
        .as_array()
        .unwrap()
        .is_empty());
    f.models.fail.store(false, Ordering::SeqCst);
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/meeting/start",
            Some(json!({"title":"Launch"}))
        )
        .await
        .0,
        200
    );
    assert_eq!(request(f.app(), "POST", "/meeting/stop", None).await.0, 200);
    let meeting = request(
        f.app(),
        "POST",
        "/api/recall",
        Some(json!({"question":"launch March","llm":false})),
    )
    .await;
    assert!(meeting.1["citations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["source"] == "meeting"));
}

#[tokio::test]
async fn work_prompts_are_saved_queued_filtered_and_transitioned_atomically() {
    let mut f = Fixture::new();
    assert_eq!(
        request(f.app(), "GET", "/prompts", None).await,
        (200, json!({"entries":[]}))
    );
    f.config.set_field("work_prompts", json!(true));
    f.models.work.store(true, Ordering::SeqCst);
    request(
        f.app(),
        "POST",
        "/meeting/start",
        Some(json!({"title":"Launch"})),
    )
    .await;
    let saved = request(f.app(), "POST", "/meeting/stop", None).await;
    assert_eq!(saved.0, 200);
    assert_eq!(saved.1["prompts"], 1);
    assert!(fs::read_to_string(saved.1["notes_path"].as_str().unwrap())
        .unwrap()
        .contains("## Suggested Work Prompts"));
    let entries = request(f.app(), "GET", "/prompts?status=pending", None)
        .await
        .1;
    let entry = &entries["entries"][0];
    assert!(Path::new(entry["path"].as_str().unwrap()).is_file());
    let id = entry["id"].as_str().unwrap();
    let approved = request(f.app(), "POST", &format!("/prompts/{id}/approve"), None).await;
    assert_eq!(approved.0, 200);
    assert_eq!(approved.1["status"], "approved");
    assert_eq!(
        request(f.app(), "POST", &format!("/prompts/{id}/approve"), None).await,
        (
            409,
            json!({"detail":"cannot approve a approved prompt (allowed from: pending, failed)"})
        )
    );
    assert_eq!(
        request(f.app(), "POST", &format!("/prompts/{id}/reject"), None)
            .await
            .0,
        409
    );
    assert_eq!(
        request(f.app(), "GET", "/prompts?status=pending", None)
            .await
            .1,
        json!({"entries":[]})
    );
    assert_eq!(
        request(f.app(), "POST", "/prompts/missing/reject", None).await,
        (404, json!({"detail":"unknown prompt: missing"}))
    );
    let queue = PromptQueue::new(f.dir.path().join("queue.json"));
    let other = queue
        .add(
            &[WorkPrompt {
                title: "Other".into(),
                body: "Test it".into(),
            }],
            &[f.dir.path().join("other.md")],
            "Launch",
        )
        .unwrap();
    let id = other[0]["id"].as_str().unwrap();
    assert_eq!(
        request(f.app(), "POST", &format!("/prompts/{id}/reject"), None)
            .await
            .1["status"],
        "rejected"
    );
    assert!(f
        .models
        .calls
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.1.contains("<output_format>") && c.3 == Some(8192)));
}

#[tokio::test]
async fn failed_note_save_releases_slot_and_failed_prompt_write_is_nonfatal() {
    let mut f = Fixture::new();
    let vault = f.dir.path().join("vault");
    fs::write(&vault, "not a directory").unwrap();
    assert_eq!(
        request(f.app(), "POST", "/meeting/start", Some(json!({})))
            .await
            .0,
        200
    );
    assert_eq!(
        request(f.app(), "POST", "/meeting/stop", None).await,
        (500, json!({"detail":"Internal Server Error"}))
    );
    fs::remove_file(vault).unwrap();
    f.config.set_field("work_prompts", json!(true));
    f.models.work.store(true, Ordering::SeqCst);
    fs::write(f.dir.path().join("prompts"), "not a directory").unwrap();
    assert_eq!(
        request(f.app(), "POST", "/meeting/start", Some(json!({})))
            .await
            .0,
        200
    );
    let saved = request(f.app(), "POST", "/meeting/stop", None).await;
    assert_eq!(saved.0, 200);
    assert!(saved.1.get("prompts").is_none());
    assert!(Path::new(saved.1["notes_path"].as_str().unwrap()).is_file());
}

#[tokio::test]
async fn recall_storage_failure_and_failed_prompt_retry_are_reported() {
    let mut f = Fixture::new();
    f.config.set_field("history_enabled", json!(true));
    fs::create_dir(f.dir.path().join("history.sqlite")).unwrap();
    let response = request(
        f.app(),
        "POST",
        "/api/recall",
        Some(json!({"question":"budget","llm":false})),
    )
    .await;
    assert_eq!(response.0, 500);
    assert!(response.1["detail"]
        .as_str()
        .unwrap()
        .starts_with("recall failed: "));
    let queue = PromptQueue::new(f.dir.path().join("queue.json"));
    let entries = queue
        .add(
            &[WorkPrompt {
                title: "Retry".into(),
                body: "Try again".into(),
            }],
            &[f.dir.path().join("retry.md")],
            "Launch",
        )
        .unwrap();
    let id = entries[0]["id"].as_str().unwrap();
    queue
        .set_status(
            id,
            "failed",
            None,
            &serde_json::Map::from_iter([
                ("error".into(), json!("previous error")),
                ("worktree".into(), json!("old")),
                ("handle".into(), json!("old")),
            ]),
        )
        .unwrap();
    let retried = request(f.app(), "POST", &format!("/prompts/{id}/approve"), None).await;
    assert_eq!(retried.0, 200);
    for key in ["error", "worktree", "handle"] {
        assert_eq!(retried.1[key], "");
    }
}

struct ChannelMenu(mpsc::Sender<lf_runtime::menubar::MenuModel>);
impl lf_runtime::menubar::MenuBar for ChannelMenu {
    fn update(&self, model: &lf_runtime::menubar::MenuModel) {
        let _ = self.0.send(model.clone());
    }
}

#[tokio::test]
async fn menu_poller_reads_live_meeting_state_and_recovers_from_config_errors() {
    use lf_runtime::menubar::{MenuState, DOWN_ICON, IDLE_ICON, LIVE_ICON};
    let f = Fixture::new();
    let (tx, rx) = mpsc::channel();
    let menu = MenuState::new(Some(Arc::new(ChannelMenu(tx))), false);
    let failed = Arc::new(AtomicBool::new(true));
    let fail_config = failed.clone();
    let config = f.config.clone();
    let poller = f.backend.start_status_poller(
        Arc::new(move || {
            if fail_config.load(Ordering::SeqCst) {
                Err("config unavailable".into())
            } else {
                Ok(config.clone())
            }
        }),
        menu.clone(),
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap().base_icon,
        DOWN_ICON
    );
    failed.store(false, Ordering::SeqCst);
    poller.poke();
    let idle = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(idle.base_icon, IDLE_ICON);
    assert!(!idle.session_active);
    assert_eq!(
        request(
            app(f.config.clone(), f.backend.clone()),
            "POST",
            "/meeting/start",
            Some(json!({"title":"Menu meeting"}))
        )
        .await
        .0,
        200
    );
    poller.poke();
    let live = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(live.base_icon, LIVE_ICON);
    assert!(live.session_active);
    assert!(live.status_line.starts_with("recording: Menu meeting — "));
    assert!(!live.dictation_enabled);
    assert_eq!(
        request(
            app(f.config.clone(), f.backend.clone()),
            "POST",
            "/meeting/stop",
            None
        )
        .await
        .0,
        200
    );
    poller.poke();
    assert!(
        !rx.recv_timeout(Duration::from_secs(5))
            .unwrap()
            .session_active
    );
    drop(poller);
    assert!(!menu.snapshot().dictation_enabled);
}

struct MockSpeech;
impl lf_runtime::engine::SpeechEngine for MockSpeech {
    fn load(&self) -> Result<(), String> {
        Ok(())
    }
    fn transcribe(&self, _: &[f32], _: Option<&str>) -> Result<String, String> {
        Ok("Dictated words".into())
    }
    fn unload(&self) {}
}

#[tokio::test]
async fn dictation_backend_composes_as_inner_through_server_router() {
    let f = Fixture::new();
    let dictation = lf_runtime::server::DictationBackend {
        speech: Arc::new(MockSpeech),
        vocabulary: Arc::new(|_| Default::default()),
        build_llm: Arc::new(|_| None),
        history: Arc::new(|_| None),
        decode: Arc::new(|_, _| Ok(vec![0.5; 16000])),
        fallback: Arc::new(Inner),
    };
    let backend = Arc::new(
        MeetingsBackend::new()
            .unwrap()
            .with_inner(Arc::new(dictation))
            .with_queue(PromptQueue::new(
                f.dir.path().join("composition-queue.json"),
            )),
    );
    assert!(backend.transcriber_loaded());
    assert!(backend.stt_available("mock"));
    assert_eq!(backend.autostart_status(), "enabled");
    assert_eq!(
        request(
            app(f.config.clone(), backend.clone()),
            "GET",
            "/meeting/status",
            None
        )
        .await
        .0,
        200
    );
    let response = app(f.config.clone(), backend.clone())
        .oneshot(Request::post("/transcribe")
            .header("host", "localhost")
            .header("x-localflow-token", "test-token")
            .header("content-type", "multipart/form-data; boundary=mock")
            .body(Body::from("--mock\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\nmock\r\n--mock--\r\n")).unwrap())
        .await.unwrap();
    assert_eq!(response.status(), 200);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(body["text"], "Dictated words");
    assert_eq!(
        request(
            app(f.config.clone(), backend),
            "GET",
            "/api/autostart",
            None
        )
        .await
        .0,
        200
    );
}

struct FailedSetup;
impl ProviderFactory for FailedSetup {
    fn llm(&self, _: &Config) -> Result<Box<dyn LlmProvider>> {
        anyhow::bail!("model setup failed")
    }
}
#[tokio::test]
async fn failed_summary_provider_setup_still_saves_recording_and_user_notes() {
    let f = Fixture::new();
    let backend = Arc::new(
        MeetingsBackend::new()
            .unwrap()
            .with_capture(f.capture.clone())
            .with_providers(Arc::new(FailedSetup))
            .with_queue(PromptQueue::new(
                f.dir.path().join("failed-setup-queue.json"),
            )),
    );
    for user_notes in ["", "## Keep this\nImportant user notes"] {
        assert_eq!(
            request(
                app(f.config.clone(), backend.clone()),
                "POST",
                "/meeting/start",
                Some(json!({"title":"Rescued","user_notes":user_notes}))
            )
            .await
            .0,
            200
        );
        let saved = request(
            app(f.config.clone(), backend.clone()),
            "POST",
            "/meeting/stop",
            None,
        )
        .await;
        assert_eq!(saved.0, 200, "{}", saved.1);
        assert_eq!(saved.1["segments"], 1);
        let notes = fs::read_to_string(saved.1["notes_path"].as_str().unwrap()).unwrap();
        assert!(notes.contains("(LLM summarization failed — see the linked transcript log.)"));
        assert!(notes.replace("\r\n", "\n").contains(user_notes));
        let transcript = fs::read_to_string(saved.1["log_path"].as_str().unwrap()).unwrap();
        assert!(transcript.contains("we will launch on March 3"));
        assert_eq!(
            request(
                app(f.config.clone(), backend.clone()),
                "GET",
                "/meeting/status",
                None
            )
            .await
            .1["last_saved"],
            saved.1
        );
    }
}

struct Inner;
impl Backend for Inner {
    fn handle(&self, request: BackendRequest) -> BackendResponse {
        BackendResponse {
            status: 202,
            body: json!({"path":request.path,"method":request.method.as_str(),"query":request.query,"body":String::from_utf8_lossy(&request.body),"model":request.config.field_str("llm_model"),"token_present":request.headers.contains_key("x-localflow-token")}),
        }
    }
    fn stt_available(&self, provider: &str) -> bool {
        provider == "mock"
    }
    fn transcriber_loaded(&self) -> bool {
        true
    }
    fn autostart_status(&self) -> String {
        "enabled".into()
    }
}
#[tokio::test]
async fn delegation_preserves_request_and_auth_remains_in_server() {
    let f = Fixture::new();
    let backend = Arc::new(MeetingsBackend::new().unwrap().with_inner(Arc::new(Inner)));
    assert!(backend.stt_available("mock") && !backend.stt_available("other"));
    assert!(backend.transcriber_loaded());
    assert_eq!(backend.autostart_status(), "enabled");
    let response = request(
        app(f.config.clone(), backend.clone()),
        "POST",
        "/transcribe?keep=yes",
        Some(json!({"audio":"mock"})),
    )
    .await;
    assert_eq!(
        response,
        (
            202,
            json!({"path":"/transcribe","method":"POST","query":{"keep":"yes"},"body":"{\"audio\":\"mock\"}","model":f.config.field_str("llm_model"),"token_present":true})
        )
    );
    assert_eq!(
        request(
            app(f.config.clone(), backend.clone()),
            "GET",
            "/api/autostart",
            None
        )
        .await,
        (200, json!({"status":"enabled"}))
    );
    assert_eq!(
        request(
            app(f.config.clone(), backend),
            "POST",
            "/api/autostart",
            Some(json!({"enabled":true}))
        )
        .await
        .0,
        202
    );
    for path in [
        "/meeting/templates",
        "/meeting/status",
        "/meeting/start",
        "/meeting/notes",
        "/meeting/chat",
        "/meeting/stop",
        "/meeting/dismiss",
        "/api/recall",
        "/prompts",
        "/prompts/id/approve",
        "/prompts/id/reject",
    ] {
        for method in ["GET", "POST"] {
            assert_eq!(
                raw(f.app(), method, path, "{}".into(), false, "localhost")
                    .await
                    .0,
                401
            );
            assert_eq!(
                raw(f.app(), method, path, "{}".into(), false, "10.0.0.5")
                    .await
                    .0,
                403
            );
        }
    }
    assert_eq!(
        request(f.app(), "POST", "/meeting/dismiss", None).await,
        (200, json!({"ok":true}))
    );
    assert_eq!(f.capture.opened.load(Ordering::SeqCst), 0);
    assert!(!MeetingsBackend::new().unwrap().stt_available("mock"));
}
