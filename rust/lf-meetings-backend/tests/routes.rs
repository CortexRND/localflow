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
    diarize::{SpeakerBackend, SpeakerTurn},
    dispatch::{PromptQueue, WorkPrompt},
    speakers::SpeakerData,
    AudioCallback, AudioSource, SystemAudioUnavailable, TimedText, TimedTranscript, Transcriber,
};
use lf_meetings_backend::{
    CaptureFactory, MeetingSave, MeetingsBackend, ProviderFactory, TitleLlm,
};
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
    /// Speaker labels: system audio carries two 1.5 s speakers and a fake backend is offered.
    speakers: AtomicBool,
    diarizer_fail: AtomicBool,
    diarizer_calls: AtomicUsize,
    speaker_gate: Arc<Mutex<Option<Gate>>>,
    /// Speaker turns end this many milliseconds early (fractional talk time).
    turn_trim_ms: Arc<AtomicUsize>,
    /// The speaker backend's `load` fails (on the collector's worker).
    speaker_load_fail: Arc<AtomicBool>,
}
struct Device {
    _local: Rc<()>,
    owner: std::thread::ThreadId,
    sample: f32,
    /// A second 1.5 s speaker after 1.5 s of `sample` (else 1 s of `sample`).
    second: Option<f32>,
    started: Arc<AtomicUsize>,
    stopped: Arc<AtomicUsize>,
}
const BLOCK: usize = 24_000;
/// One turn per 1.5 s block, speaker and one-hot embedding by amplitude.
struct Speakers(Arc<Mutex<Option<Gate>>>, Arc<AtomicUsize>, Arc<AtomicBool>);
fn bucket(sample: f32) -> usize {
    (sample * 10.0).round() as usize
}
impl SpeakerBackend for Speakers {
    fn embedder_id(&self) -> Option<&'static str> {
        Some("mock")
    }
    fn load(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.2.load(Ordering::SeqCst),
            "/private/models/x.onnx missing"
        );
        Ok(())
    }
    fn diarize(&mut self, audio: &[f32]) -> Result<Vec<SpeakerTurn>> {
        if let Some((entered, release)) = self.0.lock().unwrap().take() {
            entered.send(()).unwrap();
            release.recv_timeout(Duration::from_secs(10)).unwrap();
        }
        Ok(audio
            .chunks(BLOCK)
            .enumerate()
            .map(|(i, s)| SpeakerTurn {
                start: i as f64 * 1.5,
                end: i as f64 * 1.5 + s.len() as f64 / 16000.0
                    - self.1.load(Ordering::SeqCst) as f64 / 1000.0,
                local_speaker: bucket(s[0]),
            })
            .collect())
    }
    fn embed(&mut self, audio: &[f32]) -> Result<Vec<f32>> {
        let mut v = vec![0.0; 8];
        v[bucket(audio[0])] = 1.0;
        Ok(v)
    }
}
impl AudioSource for Device {
    fn start(
        &mut self,
        callback: AudioCallback,
    ) -> std::result::Result<(), SystemAudioUnavailable> {
        assert_eq!(self.owner, std::thread::current().id());
        self.started.fetch_add(1, Ordering::SeqCst);
        match self.second {
            None => callback(vec![self.sample; 16000]),
            Some(second) => {
                callback(vec![self.sample; BLOCK]);
                callback(vec![second; BLOCK]);
            }
        }
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
    /// Same flat text, plus one timed segment per 1.5 s block.
    fn transcribe_chunk(&self, audio: &[f32], prompt: Option<&str>) -> Result<TimedTranscript> {
        Ok(TimedTranscript {
            text: self.transcribe(audio, prompt)?,
            segments: audio
                .chunks(BLOCK)
                .enumerate()
                .map(|(i, s)| TimedText {
                    start: i as f64 * 1.5,
                    end: i as f64 * 1.5 + s.len() as f64 / 16000.0,
                    text: format!("remote part {}", i + 1),
                })
                .collect(),
        })
    }
}
impl Capture {
    fn device(&self, sample: f32) -> Box<dyn AudioSource> {
        self.device_with(sample, None)
    }
    fn device_with(&self, sample: f32, second: Option<f32>) -> Box<dyn AudioSource> {
        Box::new(Device {
            _local: Rc::new(()),
            owner: std::thread::current().id(),
            sample,
            second,
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
        let second = self.speakers.load(Ordering::SeqCst).then_some(0.4);
        Ok(Some(self.device_with(0.2, second)))
    }
    fn diarizer(&self, _: &Config) -> Result<Option<Box<dyn SpeakerBackend>>> {
        self.diarizer_calls.fetch_add(1, Ordering::SeqCst);
        anyhow::ensure!(
            !self.diarizer_fail.load(Ordering::SeqCst),
            "speaker models missing"
        );
        Ok(self.speakers.load(Ordering::SeqCst).then(|| {
            Box::new(Speakers(
                self.speaker_gate.clone(),
                self.turn_trim_ms.clone(),
                self.speaker_load_fail.clone(),
            )) as Box<dyn SpeakerBackend>
        }))
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
                .with_history_path(dir.join("history.sqlite"))
                // Never the real ~/.localflow.
                .with_speaker_data(Arc::new(SpeakerData::with_roots(
                    dir.join("meetings"),
                    dir.join("speakers"),
                )))
                .with_model_cache(dir.join("hf-hub")),
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
        json!({"watching":false,"watch_error":"","mic_busy":false,"detected":false,"platform":"","auto_start":true,"capture_system_audio":false,"speaker_diarization":false,"session":null,"last_saved":null,"chat_available":false})
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
            json!({"ok":true,"category":"Other","template":"standup","system_audio":"on","system_audio_error":"","diarization":"off","diarization_error":""})
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

fn stop_request(f: &Fixture) -> std::thread::JoinHandle<lf_server::backend::BackendResponse> {
    let backend = f.backend.clone();
    let config = f.config.clone();
    std::thread::spawn(move || {
        backend.handle(BackendRequest {
            method: "POST".parse().unwrap(),
            path: "/meeting/stop".into(),
            body: Default::default(),
            headers: Default::default(),
            query: Default::default(),
            config,
        })
    })
}

fn start_request(f: &Fixture) -> std::thread::JoinHandle<u16> {
    let (backend, config) = (f.backend.clone(), f.config.clone());
    std::thread::spawn(move || {
        let mut request = BackendRequest {
            method: "POST".parse().unwrap(),
            path: "/meeting/start".into(),
            body: "{}".into(),
            headers: Default::default(),
            query: Default::default(),
            config,
        };
        request
            .headers
            .insert("content-type", "application/json".parse().unwrap());
        backend.handle(request).status
    })
}

#[tokio::test]
async fn saved_transcript_wait_ignores_an_earlier_meetings_save() {
    let f = Fixture::new();
    for title in ["First", "Second"] {
        let body = json!({"title": title, "auto_title": false});
        assert_eq!(
            request(f.app(), "POST", "/meeting/start", Some(body))
                .await
                .0,
            200
        );
        if title == "First" {
            assert_eq!(request(f.app(), "POST", "/meeting/stop", None).await.0, 200);
            assert_eq!(f.backend.wait_for_saved_transcript(), MeetingSave::Note);
        }
    }
    let (finished, done) = mpsc::channel();
    let backend = f.backend.clone();
    let waiter =
        std::thread::spawn(move || finished.send(backend.wait_for_saved_transcript()).unwrap());
    assert!(done.recv_timeout(Duration::from_millis(50)).is_err());
    stop_request(&f).join().unwrap();
    assert!(matches!(
        done.recv_timeout(Duration::from_secs(5)).unwrap(),
        MeetingSave::Transcript | MeetingSave::Note
    ));
    waiter.join().unwrap();
    let saved = request(f.app(), "GET", "/meeting/status", None).await.1["last_saved"].clone();
    let log = saved["log_path"].as_str().unwrap();
    assert!(log.contains("Second"));
    assert!(fs::read_to_string(log)
        .unwrap()
        .contains("we will launch on March 3"));
}

#[tokio::test]
async fn a_starting_meeting_is_active_and_a_failed_start_leaves_no_meeting() {
    let f = Fixture::new();
    let (entered, loading) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *f.capture.gate.lock().unwrap() = Some((entered, released));
    f.capture.stt_fail.store(true, Ordering::SeqCst);
    let start = start_request(&f);
    loading.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(f.backend.session_active());
    let (finished, done) = mpsc::channel();
    let backend = f.backend.clone();
    let waiter =
        std::thread::spawn(move || finished.send(backend.wait_for_saved_transcript()).unwrap());
    assert!(done.recv_timeout(Duration::from_millis(50)).is_err());
    release.send(()).unwrap();
    assert_eq!(start.join().unwrap(), 500);
    assert_eq!(
        done.recv_timeout(Duration::from_secs(5)).unwrap(),
        MeetingSave::NoMeeting
    );
    waiter.join().unwrap();
    assert!(!f.backend.session_active());
}

#[tokio::test]
async fn a_wait_begun_while_starting_ends_at_that_meetings_transcript() {
    let f = Fixture::new();
    let (entered, loading) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *f.capture.gate.lock().unwrap() = Some((entered, released));
    let start = start_request(&f);
    loading.recv_timeout(Duration::from_secs(5)).unwrap();
    let (finished, done) = mpsc::channel();
    let backend = f.backend.clone();
    let waiter =
        std::thread::spawn(move || finished.send(backend.wait_for_saved_transcript()).unwrap());
    assert!(done.recv_timeout(Duration::from_millis(50)).is_err());
    release.send(()).unwrap();
    assert_eq!(start.join().unwrap(), 200);
    assert!(done.recv_timeout(Duration::from_millis(50)).is_err());
    let (entered, summarizing) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *f.models.gate.lock().unwrap() = Some((entered, released));
    let stop = stop_request(&f);
    summarizing.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        done.recv_timeout(Duration::from_secs(5)).unwrap(),
        MeetingSave::Transcript
    );
    release.send(()).unwrap();
    assert_eq!(stop.join().unwrap().status, 500);
    waiter.join().unwrap();
}

#[tokio::test]
async fn abandoned_stop_keeps_the_recovery_note_and_publishes_nothing_more() {
    let f = Fixture::new();
    assert_eq!(
        request(
            f.app(),
            "POST",
            "/meeting/start",
            Some(json!({"title":"Abandoned", "auto_title":false}))
        )
        .await
        .0,
        200
    );
    let (entered, summarizing) = mpsc::channel();
    let (release, released) = mpsc::channel();
    *f.models.gate.lock().unwrap() = Some((entered, released));
    let stop = stop_request(&f);
    summarizing.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        f.backend.wait_for_saved_transcript(),
        MeetingSave::Transcript
    );
    release.send(()).unwrap();
    assert_eq!(stop.join().unwrap().status, 500);
    let saved = request(f.app(), "GET", "/meeting/status", None).await.1["last_saved"].clone();
    assert_eq!(saved["recovery"], true);
    assert!(fs::read_to_string(saved["log_path"].as_str().unwrap())
        .unwrap()
        .contains("we will launch on March 3"));
    let notes_dir = Path::new(saved["notes_path"].as_str().unwrap())
        .parent()
        .unwrap();
    assert_eq!(fs::read_dir(notes_dir).unwrap().count(), 1);
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

fn speaker_fixture(diarization: bool) -> Fixture {
    let mut f = Fixture::new();
    f.config.set_field("capture_system_audio", json!(true));
    f.config
        .set_field("speaker_diarization", json!(diarization));
    f.capture.system.store(true, Ordering::SeqCst);
    f.capture.speakers.store(true, Ordering::SeqCst);
    f
}
async fn start_meeting(f: &Fixture) {
    let start = request(
        f.app(),
        "POST",
        "/meeting/start",
        Some(json!({"title":"Speakers", "auto_title":false})),
    )
    .await;
    assert_eq!(start.0, 200, "{}", start.1);
}

#[tokio::test]
async fn speaker_labels_are_saved_after_a_channel_labelled_recovery_checkpoint() {
    let f = speaker_fixture(true);
    start_meeting(&f).await;
    let (diarizing, release) = gate(&f.capture.speaker_gate);
    let stop = stop_request(&f);
    diarizing.recv_timeout(Duration::from_secs(5)).unwrap();
    // Finalize is blocked; the full transcript is already durable with channel labels.
    let (mut recovery, mut recovery_log) = (String::new(), String::new());
    for _ in 0..500 {
        let last = request(f.app(), "GET", "/meeting/status", None).await.1["last_saved"].clone();
        if last["recovery"] == true {
            recovery_log = last["log_path"].as_str().unwrap().to_string();
            recovery = fs::read_to_string(&recovery_log).unwrap_or_default();
            if recovery.contains("Dana owns the blog post") {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        recovery.contains("**Others:** sounds good, Dana owns the blog post"),
        "{recovery}"
    );
    assert!(recovery.contains("**Me:** we will launch on March 3"));
    assert!(!recovery.contains("Speaker 1") && !recovery.contains("remote part"));
    release.send(()).unwrap();
    let saved = stop.join().unwrap();
    assert_eq!(saved.status, 200, "{}", saved.body);
    assert_eq!(saved.body["segments"], 3);
    let log = fs::read_to_string(saved.body["log_path"].as_str().unwrap()).unwrap();
    assert!(
        log.contains("**00:00** **Me:** we will launch on March 3"),
        "{log}"
    );
    assert!(
        log.contains("**00:00** **Speaker 1:** remote part 1"),
        "{log}"
    );
    assert!(
        log.contains("**00:01** **Speaker 2:** remote part 2"),
        "{log}"
    );
    assert!(!log.contains("**Others:**"));
    assert!(
        log.contains("speakers: [\"Me\", \"Speaker 1\", \"Speaker 2\"]"),
        "{log}"
    );
    assert!(!Path::new(&recovery_log).exists());
    let prompt = f.models.calls.lock().unwrap()[0].2.clone();
    assert!(prompt.contains("Speaker 2: remote part 2"), "{prompt}");
    assert_eq!(f.capture.diarizer_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn speaker_labels_are_skipped_when_the_channel_checkpoint_cannot_be_saved() {
    let f = speaker_fixture(true);
    // A vault path that is a file: every write fails, the full checkpoint included.
    fs::write(f.dir.path().join("vault"), "not a directory").unwrap();
    start_meeting(&f).await;
    let saved = request(f.app(), "POST", "/meeting/stop", None).await;
    assert_eq!(saved, (500, json!({"detail":"Internal Server Error"})));
    assert_eq!(f.capture.diarizer_calls.load(Ordering::SeqCst), 1);
    // Stop still ran its channel path: the summary saw Me/Others, never labels.
    let calls = f.models.calls.lock().unwrap();
    let prompt = &calls[0].2;
    assert!(
        prompt.contains("Others: sounds good, Dana owns the blog post"),
        "{prompt}"
    );
    assert!(
        !prompt.contains("Speaker 1") && !prompt.contains("remote part"),
        "{prompt}"
    );
}

#[tokio::test]
async fn speaker_labels_flag_off_never_asks_for_a_diarizer_and_output_is_channel() {
    let f = speaker_fixture(false);
    start_meeting(&f).await;
    let saved = request(f.app(), "POST", "/meeting/stop", None).await;
    assert_eq!(saved.0, 200, "{}", saved.1);
    assert_eq!(saved.1["segments"], 2);
    let log = fs::read_to_string(saved.1["log_path"].as_str().unwrap()).unwrap();
    assert!(log.contains("**00:00** **Others:** sounds good, Dana owns the blog post"));
    assert!(!log.contains("Speaker 1") && !log.contains("remote part"));
    assert!(!log.contains("speakers:"));
    assert_eq!(f.capture.diarizer_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn speaker_labels_diarizer_error_still_starts_and_saves_channel_labels() {
    let f = speaker_fixture(true);
    f.capture.diarizer_fail.store(true, Ordering::SeqCst);
    start_meeting(&f).await;
    let saved = request(f.app(), "POST", "/meeting/stop", None).await;
    assert_eq!(saved.0, 200, "{}", saved.1);
    let log = fs::read_to_string(saved.1["log_path"].as_str().unwrap()).unwrap();
    assert!(log.contains("**Others:** sounds good, Dana owns the blog post"));
    assert!(!log.contains("Speaker 1"));
    assert_eq!(f.capture.diarizer_calls.load(Ordering::SeqCst), 1);
    assert_eq!(saved.1["diarization"], "off");
    assert!(!f.dir.path().join("meetings").exists());
}

// ---------------------------------------------------------------- R4 ---

async fn typed(
    app: Router,
    method: &str,
    path: &str,
    body: Vec<u8>,
    content_type: &str,
) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("Host", "localhost")
        .header("Content-Type", content_type)
        .header("X-Localflow-Token", "test-token")
        .body(Body::from(body))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}
fn direct(f: &Fixture, method: &str, path: &str, body: Value, local: bool) -> BackendResponse {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    f.backend.handle_local(
        BackendRequest {
            method: method.parse().unwrap(),
            path: path.into(),
            query: Default::default(),
            headers,
            body: body.to_string().into(),
            config: f.config.clone(),
        },
        local,
    )
}
fn sidecar(f: &Fixture, id: &str) -> Value {
    let path = f.dir.path().join("meetings").join(format!("{id}.json"));
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}
fn write_sidecar(f: &Fixture, id: &str, record: &Value) {
    let path = f.dir.path().join("meetings").join(format!("{id}.json"));
    fs::write(path, serde_json::to_vec(record).unwrap()).unwrap();
}
fn files(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.insert(path.clone(), fs::read(path).unwrap());
            }
        }
    }
    out
}
async fn stopped_meeting(f: &Fixture) -> Value {
    start_meeting(f).await;
    let saved = request(f.app(), "POST", "/meeting/stop", None).await;
    assert_eq!(saved.0, 200, "{}", saved.1);
    saved.1
}
fn prompts(f: &Fixture) -> String {
    let calls = f.models.calls.lock().unwrap();
    calls.iter().map(|c| format!("{}\n{}", c.1, c.2)).collect()
}

#[tokio::test]
async fn flag_off_stop_payload_only_adds_the_speaker_keys_and_writes_no_speaker_files() {
    let f = speaker_fixture(false);
    let saved = stopped_meeting(&f).await;
    let keys: Vec<&str> = saved
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "title",
            "title_generated",
            "notes_path",
            "log_path",
            "segments",
            "template",
            "enhanced",
            "meeting_id",
            "diarization",
            "speakers"
        ]
    );
    assert_eq!(saved["meeting_id"], Value::Null);
    assert_eq!(saved["diarization"], "off");
    assert_eq!(saved["speakers"], json!([]));
    let log = fs::read_to_string(saved["log_path"].as_str().unwrap()).unwrap();
    assert!(!log.contains("meeting_id") && !log.contains("speakers:"));
    assert!(!f.dir.path().join("meetings").exists());
    assert!(!f.dir.path().join("speakers").exists());
}

#[tokio::test]
async fn diarized_stop_saves_a_sidecar_without_voice_data_and_serves_public_speakers() {
    let f = speaker_fixture(true);
    start_meeting(&f).await;
    let live = request(f.app(), "GET", "/meeting/status", None).await.1;
    assert_eq!(live["speaker_diarization"], true);
    assert_eq!(live["session"]["diarization"], "collecting");
    assert_eq!(live["session"]["diarization_error"], "");
    let saved = request(f.app(), "POST", "/meeting/stop", None).await.1;
    let id = saved["meeting_id"].as_str().unwrap().to_string();
    assert!(lf_meetings::speakers::store::valid_meeting_id(&id));
    assert_eq!(saved["diarization"], "ok");
    let public = json!([
        {"id":"S1","label":"Speaker 1","name":null,"source":"others","seconds":1.5},
        {"id":"S2","label":"Speaker 2","name":null,"source":"others","seconds":1.5},
    ]);
    assert_eq!(saved["speakers"], public);
    let record = sidecar(&f, &id);
    assert_eq!(record["schema_version"], 1);
    assert_eq!(record["embedder"], "mock");
    assert_eq!(record["status"], "ok");
    assert_eq!(record["log_path"], saved["log_path"]);
    assert_eq!(record["notes_path"], saved["notes_path"]);
    for sp in record["speakers"].as_array().unwrap() {
        assert!(
            sp["centroid"].is_null() && sp["voiceprint_id"].is_null(),
            "{sp}"
        );
    }
    for seg in record["segments"].as_array().unwrap() {
        assert!(
            seg.get("text").is_none(),
            "no transcript text in the sidecar"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meetings = f.dir.path().join("meetings");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&meetings), 0o700);
        assert_eq!(mode(&meetings.join(format!("{id}.json"))), 0o600);
    }
    assert!(
        !f.dir.path().join("speakers").exists(),
        "recognition off: no voiceprint folder"
    );
    assert_eq!(
        request(
            f.app(),
            "GET",
            &format!("/api/meetings/{id}/speakers"),
            None
        )
        .await,
        (200, json!({"meeting_id": id, "speakers": public}))
    );
    assert_eq!(
        request(f.app(), "GET", "/meeting/status", None).await.1["last_saved"],
        saved
    );
    // The meeting id and sidecar never reach a prompt.
    let prompts = prompts(&f);
    assert!(!prompts.contains(&id) && !prompts.contains("meetings/"));
    for path in [
        "/api/meetings/x/speakers",
        &format!("/api/meetings/{}/speakers", "0".repeat(32)),
    ] {
        assert_eq!(
            request(f.app(), "GET", path, None).await,
            (404, json!({"detail":"no speaker data for this meeting"}))
        );
    }
}

#[tokio::test]
async fn unavailable_speaker_labels_are_reported_live() {
    let f = speaker_fixture(true);
    f.capture.diarizer_fail.store(true, Ordering::SeqCst);
    let start = request(
        f.app(),
        "POST",
        "/meeting/start",
        Some(json!({"title":"Speakers", "auto_title":false})),
    )
    .await
    .1;
    assert_eq!(start["diarization"], "unavailable");
    assert_eq!(start["diarization_error"], "speaker models missing");
    let live = request(f.app(), "GET", "/meeting/status", None).await.1;
    assert_eq!(live["session"]["diarization"], "unavailable");
}

#[tokio::test]
async fn rename_and_merge_rewrite_only_speaker_prefixes_frontmatter_and_sidecar() {
    let f = speaker_fixture(true);
    let saved = stopped_meeting(&f).await;
    let id = saved["meeting_id"].as_str().unwrap().to_string();
    let log_path = std::path::PathBuf::from(saved["log_path"].as_str().unwrap());
    let vault = f.dir.path().join("vault");
    let before = files(&vault);
    let base = format!("/api/meetings/{id}/speakers");
    for (method, path, body, expected) in [
        (
            "PUT",
            format!("{base}/S2"),
            Some(json!({"name":"!!!"})),
            (400, json!({"detail":"invalid speaker name"})),
        ),
        (
            "PUT",
            format!("{base}/S2"),
            Some(json!({"name":7})),
            (400, json!({"detail":"invalid speaker name"})),
        ),
        (
            "PUT",
            format!("{base}/S9"),
            Some(json!({"name":"Dana"})),
            (404, json!({"detail":"unknown speaker"})),
        ),
        (
            "PUT",
            format!("/api/meetings/{}/speakers/S2", "0".repeat(32)),
            Some(json!({"name":"Dana"})),
            (404, json!({"detail":"no speaker data for this meeting"})),
        ),
        (
            "POST",
            format!("{base}/merge"),
            Some(json!({"from":"S1","into":"S1"})),
            (
                400,
                json!({"detail":"from and into must be two different speaker ids"}),
            ),
        ),
        (
            "POST",
            format!("{base}/merge"),
            Some(json!({"from":"S1"})),
            (
                400,
                json!({"detail":"from and into must be two different speaker ids"}),
            ),
        ),
        (
            "POST",
            format!("{base}/merge"),
            Some(json!({"from":"S1","into":"S7"})),
            (404, json!({"detail":"unknown speaker"})),
        ),
    ] {
        assert_eq!(
            request(f.app(), method, &path, body).await,
            expected,
            "{method} {path}"
        );
    }
    let missing = request(f.app(), "PUT", &format!("{base}/S2"), None).await;
    assert_eq!(missing.0, 422);
    assert_eq!(files(&vault), before, "failed edits write nothing");

    let renamed = request(
        f.app(),
        "PUT",
        &format!("{base}/S2"),
        Some(json!({"name":"  Dana  O'Neil "})),
    )
    .await;
    assert_eq!(renamed.0, 200, "{}", renamed.1);
    assert_eq!(renamed.1["speakers"][1]["label"], "Dana O'Neil");
    assert_eq!(renamed.1["speakers"][1]["name"], "Dana O'Neil");
    let log = fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("**00:01** **Dana O'Neil:** remote part 2"),
        "{log}"
    );
    assert!(
        log.contains("**00:00** **Speaker 1:** remote part 1"),
        "{log}"
    );
    assert!(
        log.contains("speakers: [\"Me\", \"Speaker 1\", \"Dana O'Neil\"]"),
        "{log}"
    );
    assert!(log.contains("**00:00** **Me:** we will launch on March 3"));
    let mut after = files(&vault);
    after.remove(&log_path);
    let mut unchanged = before.clone();
    unchanged.remove(&log_path);
    assert_eq!(
        after, unchanged,
        "only the transcript changes; the notes file never"
    );
    assert_eq!(sidecar(&f, &id)["speakers"][1]["label"], "Dana O'Neil");
    assert_eq!(
        request(f.app(), "GET", "/meeting/status", None).await.1["last_saved"]["speakers"],
        renamed.1["speakers"]
    );
    // The same label again: nothing is rewritten.
    let bytes = fs::read(&log_path).unwrap();
    assert_eq!(
        request(
            f.app(),
            "PUT",
            &format!("{base}/S2"),
            Some(json!({"name":"Dana O'Neil"}))
        )
        .await
        .0,
        200
    );
    assert_eq!(fs::read(&log_path).unwrap(), bytes);

    let merged = request(
        f.app(),
        "POST",
        &format!("{base}/merge"),
        Some(json!({"from":"S1","into":"S2"})),
    )
    .await;
    assert_eq!(
        merged,
        (
            200,
            json!({"meeting_id": id, "speakers": [
                {"id":"S2","label":"Dana O'Neil","name":"Dana O'Neil","source":"others","seconds":3.0}
            ]})
        )
    );
    let log = fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("**00:00** **Dana O'Neil:** remote part 1"),
        "{log}"
    );
    assert!(log.contains("speakers: [\"Me\", \"Dana O'Neil\"]"), "{log}");
    assert!(!sidecar(&f, &id)["segments"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["speaker_id"] == "S1"));

    // A transcript edited since: refused, nothing written.
    // Only the line's contents are removed, so the edit works with either
    // line ending (the transcript is CRLF on Windows).
    let line = "**00:01** **Dana O'Neil:** remote part 2";
    assert_eq!(log.matches(line).count(), 1, "{log}");
    let edited = log.replace(line, "");
    assert_ne!(edited, log, "the simulated edit removed the line");
    assert!(!edited.contains(line));
    fs::write(&log_path, &edited).unwrap();
    let record = sidecar(&f, &id);
    assert_eq!(
        request(
            f.app(),
            "PUT",
            &format!("{base}/S2"),
            Some(json!({"name":"Dee"}))
        )
        .await,
        (
            409,
            json!({"detail":"transcript lines do not match the speaker data"})
        )
    );
    assert_eq!(fs::read_to_string(&log_path).unwrap(), edited);
    assert_eq!(sidecar(&f, &id), record);

    // Fail closed on the sidecar's transcript path.
    let mut moved = record.clone();
    moved["log_path"] = saved["notes_path"].clone();
    write_sidecar(&f, &id, &moved);
    assert_eq!(
        request(
            f.app(),
            "PUT",
            &format!("{base}/S2"),
            Some(json!({"name":"Dee"}))
        )
        .await,
        (
            409,
            json!({"detail":"transcript is the meeting notes file"})
        )
    );
    let outside = f.dir.path().join("outside.md");
    fs::write(&outside, &edited).unwrap();
    moved["log_path"] = json!(outside);
    write_sidecar(&f, &id, &moved);
    assert_eq!(
        request(
            f.app(),
            "PUT",
            &format!("{base}/S2"),
            Some(json!({"name":"Dee"}))
        )
        .await,
        (
            409,
            json!({"detail":"transcript is outside the meeting vault"})
        )
    );
    moved["log_path"] = json!(vault.join("gone.md"));
    write_sidecar(&f, &id, &moved);
    assert_eq!(
        request(
            f.app(),
            "PUT",
            &format!("{base}/S2"),
            Some(json!({"name":"Dee"}))
        )
        .await,
        (404, json!({"detail":"transcript file not found"}))
    );
    assert_eq!(fs::read_to_string(&outside).unwrap(), edited);
}

#[tokio::test]
async fn new_routes_need_the_token_and_voiceprint_routes_the_recognition_gate() {
    let f = speaker_fixture(true);
    let routes = [
        ("GET", "/api/speakers"),
        ("POST", "/api/speakers"),
        ("DELETE", "/api/speakers"),
        ("GET", "/api/speakers/export"),
        ("PATCH", "/api/speakers/Dana"),
        ("DELETE", "/api/speakers/Dana"),
    ];
    let meeting = format!("/api/meetings/{}/speakers", "a".repeat(32));
    for (method, path) in routes.iter().copied().chain([
        ("GET", meeting.as_str()),
        ("PUT", &*format!("{meeting}/S1")),
        ("POST", &*format!("{meeting}/merge")),
        ("GET", "/api/diarization/status"),
    ]) {
        assert_eq!(
            raw(f.app(), method, path, "{}".into(), false, "localhost").await,
            (401, json!({"detail":"invalid API token"})),
            "{method} {path}"
        );
    }
    for (method, path) in routes {
        assert_eq!(
            request(f.app(), method, path, Some(json!({}))).await,
            (403, json!({"detail":"speaker recognition is off"})),
            "{method} {path}"
        );
    }
    // The gate comes before the body: an oversized upload is still 403.
    assert_eq!(
        raw(
            f.app(),
            "POST",
            "/api/speakers",
            "x".repeat(200_000),
            true,
            "localhost"
        )
        .await,
        (403, json!({"detail":"speaker recognition is off"}))
    );
    assert_eq!(
        request(f.app(), "POST", "/api/speakers/x", None).await.0,
        405
    );
    let status = request(f.app(), "GET", "/api/diarization/status", None).await;
    assert_eq!(status.0, 200);
    let keys: Vec<&str> = status
        .1
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        [
            "models_ready",
            "reason",
            "extra_installed",
            "models",
            "speaker_diarization",
            "diarize_mic",
            "speaker_recognition",
            "download"
        ]
    );
    assert_eq!(status.1["models_ready"], false);
    assert_eq!(status.1["speaker_diarization"], true);
    assert_eq!(
        status.1["extra_installed"],
        lf_meetings::diarize::models::RUNTIME_INSTALLED
    );
    assert_eq!(
        status.1["download"],
        json!({"state":"idle","done_bytes":0,"total_bytes":0,"error":null})
    );
    assert_eq!(
        status.1["models"][1]["id"],
        "3dspeaker-campplus-zh-en-16k-advanced"
    );
}

#[tokio::test]
async fn recognition_enrolls_names_the_next_meeting_exports_locally_and_forgets() {
    let mut f = speaker_fixture(true);
    f.config.set_field("speaker_recognition", json!(true));
    f.config.set_field("diarization_backend", json!("mock"));
    let first = stopped_meeting(&f).await;
    let id = first["meeting_id"].as_str().unwrap().to_string();
    let record = sidecar(&f, &id);
    let centroid = &record["speakers"][1]["centroid"];
    assert_eq!(centroid.as_array().unwrap().len(), 8);
    assert_eq!(centroid[4], 1.0);
    // Public routes and payloads never carry voice data.
    for body in [
        first.to_string(),
        request(
            f.app(),
            "GET",
            &format!("/api/meetings/{id}/speakers"),
            None,
        )
        .await
        .1
        .to_string(),
    ] {
        assert!(
            !body.contains("centroid") && !body.contains("voiceprint"),
            "{body}"
        );
    }
    assert_eq!(
        request(f.app(), "GET", "/api/speakers", None).await,
        (200, json!({"speakers":[],"embedder":"mock"}))
    );
    let enroll = |body: Value| {
        let app = f.app();
        async move { request(app, "POST", "/api/speakers", Some(body)).await }
    };
    for (body, expected) in [
        (
            json!({"name":"Dana","meeting_id":id}),
            (
                400,
                json!({"detail":"meeting_id and speaker_id are required"}),
            ),
        ),
        (
            json!({"name":"!","meeting_id":id,"speaker_id":"S2"}),
            (400, json!({"detail":"invalid speaker name"})),
        ),
        (
            json!({"name":"Dana","meeting_id":"0".repeat(32),"speaker_id":"S2"}),
            (404, json!({"detail":"no speaker data for this meeting"})),
        ),
        (
            json!({"name":"Dana","meeting_id":id,"speaker_id":"S9"}),
            (404, json!({"detail":"unknown speaker"})),
        ),
        (
            json!([1]),
            (400, json!({"detail":"expected a JSON object"})),
        ),
    ] {
        assert_eq!(enroll(body.clone()).await, expected, "{body}");
    }
    assert_eq!(
        typed(
            f.app(),
            "POST",
            "/api/speakers",
            b"{".to_vec(),
            "application/json"
        )
        .await,
        (400, json!({"detail":"invalid JSON"}))
    );
    assert_eq!(
        typed(
            f.app(),
            "POST",
            "/api/speakers",
            b"hi".to_vec(),
            "text/plain"
        )
        .await,
        (
            415,
            json!({"detail":"send JSON or a multipart audio upload"})
        )
    );
    assert_eq!(
        typed(
            f.app(),
            "POST",
            "/api/speakers",
            vec![b' '; 70_000],
            "application/json"
        )
        .await,
        (413, json!({"detail":"upload too large"}))
    );
    // Without a known loopback peer (no ConnectInfo) nothing is local.
    assert_eq!(
        typed(
            f.app(),
            "POST",
            "/api/speakers",
            b"--x--\r\n".to_vec(),
            "multipart/form-data; boundary=x"
        )
        .await,
        (
            403,
            json!({"detail":"enrolling from audio is only available on this computer"})
        )
    );
    assert_eq!(
        request(f.app(), "GET", "/api/speakers/export", None).await,
        (
            403,
            json!({"detail":"export is only available on this computer"})
        )
    );

    let created = enroll(json!({"name":"Dana","meeting_id":id,"speaker_id":"S2"})).await;
    assert_eq!(created.0, 201, "{}", created.1);
    let vp = created.1["id"].as_str().unwrap().to_string();
    assert_eq!(created.1["name"], "Dana");
    assert_eq!(created.1["model_id"], "mock");
    assert_eq!(created.1["samples"], 1);
    assert!(created.1["created"].as_str().unwrap().ends_with('Z'));
    assert_eq!(
        sidecar(&f, &id)["speakers"][1]["voiceprint_id"],
        vp.as_str()
    );
    let speakers_dir = f.dir.path().join("speakers");
    let registry = fs::read_to_string(speakers_dir.join("registry.json")).unwrap();
    assert!(
        !registry.contains("1.0"),
        "no vector in registry.json: {registry}"
    );
    assert!(speakers_dir.join(format!("{vp}.npy")).is_file());

    // The next meeting names the enrolled voice.
    let second = stopped_meeting(&f).await;
    assert_eq!(
        second["speakers"][1],
        json!({"id":"S2","label":"Dana","name":"Dana","source":"others","seconds":1.5})
    );
    assert_eq!(second["speakers"][0]["name"], Value::Null);
    let log = fs::read_to_string(second["log_path"].as_str().unwrap()).unwrap();
    assert!(log.contains("**00:01** **Dana:** remote part 2"), "{log}");
    let second_id = second["meeting_id"].as_str().unwrap().to_string();
    assert_eq!(
        sidecar(&f, &second_id)["speakers"][1]["voiceprint_id"],
        vp.as_str()
    );
    let prompts = prompts(&f);
    assert!(prompts.contains("Dana: remote part 2"));
    assert!(!prompts.contains(&vp) && !prompts.contains(&second_id));

    // Export: local only, through the binary seam.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("host", "127.0.0.1".parse().unwrap());
    let export = |local| {
        f.backend.download(
            BackendRequest {
                method: "GET".parse().unwrap(),
                path: "/api/speakers/export".into(),
                query: Default::default(),
                headers: headers.clone(),
                body: Default::default(),
                config: f.config.clone(),
            },
            local,
        )
    };
    assert_eq!(export(false).err().unwrap().status, 403);
    let zip = export(true).ok().unwrap();
    assert_eq!(zip.content_type, "application/zip");
    assert!(zip.headers.contains(&("cache-control", "no-store".into())));
    let bundle = lf_meetings::speakers::bundle::read(&zip.body).unwrap();
    assert_eq!(bundle.entries, vec![created.1.clone()]);

    // Rename by name, then ambiguity and forget.
    let renamed = request(
        f.app(),
        "PATCH",
        "/api/speakers/dana",
        Some(json!({"name":"Dana S"})),
    )
    .await;
    assert_eq!(renamed.0, 200, "{}", renamed.1);
    assert_eq!(renamed.1["name"], "Dana S");
    assert_eq!(renamed.1["id"], vp.as_str());
    assert_eq!(
        request(
            f.app(),
            "PATCH",
            "/api/speakers/nobody",
            Some(json!({"name":"X"}))
        )
        .await,
        (404, json!({"detail":"unknown speaker"}))
    );
    assert_eq!(
        enroll(json!({"name":"Dana S","meeting_id":id,"speaker_id":"S1"}))
            .await
            .0,
        201
    );
    assert_eq!(
        request(f.app(), "DELETE", "/api/speakers/Dana%20S", None).await,
        (
            409,
            json!({"detail":"more than one enrolled speaker has this name; use its id (lf speakers list --json)"})
        )
    );
    assert_eq!(
        request(f.app(), "DELETE", &format!("/api/speakers/{vp}"), None).await,
        (200, json!({"forgotten":1}))
    );
    assert!(!speakers_dir.join(format!("{vp}.npy")).exists());
    assert_eq!(
        request(f.app(), "DELETE", "/api/speakers", None).await,
        (200, json!({"forgotten":1}))
    );
    for meeting in [&id, &second_id] {
        for sp in sidecar(&f, meeting)["speakers"].as_array().unwrap() {
            assert!(
                sp["centroid"].is_null() && sp["voiceprint_id"].is_null(),
                "{sp}"
            );
        }
    }
    assert!(fs::read_dir(&speakers_dir).unwrap().next().is_none());
    // Names already in transcripts stay; recognition off stops all reads.
    assert!(fs::read_to_string(second["log_path"].as_str().unwrap())
        .unwrap()
        .contains("**Dana:**"));
    f.config.set_field("speaker_recognition", json!(false));
    assert_eq!(request(f.app(), "GET", "/api/speakers", None).await.0, 403);
}

#[tokio::test]
async fn a_forget_after_finalize_keeps_voice_data_out_of_that_sidecar() {
    let mut f = speaker_fixture(true);
    f.config.set_field("speaker_recognition", json!(true));
    start_meeting(&f).await;
    let (summarizing, release) = gate(&f.models.gate);
    let stop = stop_request(&f);
    summarizing.recv_timeout(Duration::from_secs(5)).unwrap();
    // Clustering (with centroids) is done; the note is not saved yet.
    assert_eq!(
        direct(&f, "DELETE", "/api/speakers", json!({}), true).body,
        json!({"forgotten":0})
    );
    release.send(()).unwrap();
    let saved = stop.join().unwrap();
    assert_eq!(saved.status, 200, "{}", saved.body);
    let record = sidecar(&f, saved.body["meeting_id"].as_str().unwrap());
    assert_eq!(record["speakers"].as_array().unwrap().len(), 2);
    for sp in record["speakers"].as_array().unwrap() {
        assert!(
            sp["centroid"].is_null(),
            "no stale centroid after forget: {sp}"
        );
    }
}

#[tokio::test]
async fn the_live_gate_is_rechecked_after_the_body_is_read() {
    let mut f = speaker_fixture(true);
    f.config.set_field("speaker_recognition", json!(true));
    let saved = stopped_meeting(&f).await;
    let id = saved["meeting_id"].as_str().unwrap();
    // Same backend data, but the live config has recognition off again.
    let off = MeetingsBackend::new()
        .unwrap()
        .with_speaker_data(Arc::new(SpeakerData::with_roots(
            f.dir.path().join("meetings"),
            f.dir.path().join("speakers"),
        )))
        .with_config_reader(Arc::new(|| Ok(Config::default())));
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("content-type", "application/json".parse().unwrap());
    let response = off.handle_local(
        BackendRequest {
            method: "POST".parse().unwrap(),
            path: "/api/speakers".into(),
            query: Default::default(),
            headers,
            body: json!({"name":"Dana","meeting_id":id,"speaker_id":"S2"})
                .to_string()
                .into(),
            config: f.config.clone(),
        },
        true,
    );
    assert_eq!(response.status, 403);
    assert!(!f.dir.path().join("speakers").exists());
}

#[tokio::test]
async fn quick_exit_waits_for_an_in_flight_final_sidecar_but_not_for_slow_work() {
    let f = speaker_fixture(true);
    // The sidecar write's live-gate read is the stop's only config read
    // (recognition was off at start): block it there, armed after the
    // start's own read of the live diarization settings.
    let slot: Arc<Mutex<Option<Gate>>> = Arc::default();
    let (reader_slot, config) = (slot.clone(), f.config.clone());
    let backend = Arc::new(
        MeetingsBackend::new()
            .unwrap()
            .with_capture(f.capture.clone())
            .with_providers(Arc::new(Providers(f.models.clone())))
            .with_queue(PromptQueue::new(f.dir.path().join("queue.json")))
            .with_history_path(f.dir.path().join("history.sqlite"))
            .with_speaker_data(Arc::new(SpeakerData::with_roots(
                f.dir.path().join("meetings"),
                f.dir.path().join("speakers"),
            )))
            .with_config_reader(Arc::new(move || {
                if let Some((entered, release)) = reader_slot.lock().unwrap().take() {
                    entered.send(()).unwrap();
                    release.recv_timeout(Duration::from_secs(10)).unwrap();
                }
                Ok(config.clone())
            })),
    );
    let start = request(
        app(f.config.clone(), backend.clone()),
        "POST",
        "/meeting/start",
        Some(json!({"title":"Speakers", "auto_title":false})),
    )
    .await;
    assert_eq!(start.0, 200, "{}", start.1);
    let (writing, release) = gate(&slot);
    let (stopping, config) = (backend.clone(), f.config.clone());
    let stop = std::thread::spawn(move || {
        stopping.handle(BackendRequest {
            method: "POST".parse().unwrap(),
            path: "/meeting/stop".into(),
            body: Default::default(),
            headers: Default::default(),
            query: Default::default(),
            config,
        })
    });
    writing.recv_timeout(Duration::from_secs(10)).unwrap();
    // The final note is on disk; its sidecar is being written.
    let (done, finished) = mpsc::channel();
    let waiting = backend.clone();
    let waiter =
        std::thread::spawn(move || done.send(waiting.wait_for_saved_transcript()).unwrap());
    assert!(finished.recv_timeout(Duration::from_millis(100)).is_err());
    release.send(()).unwrap();
    assert_eq!(
        finished.recv_timeout(Duration::from_secs(10)).unwrap(),
        MeetingSave::Note
    );
    let sidecars = fs::read_dir(f.dir.path().join("meetings")).unwrap().count();
    assert_eq!(sidecars, 1, "the sidecar is complete when Note is reported");
    waiter.join().unwrap();
    assert_eq!(stop.join().unwrap().status, 200);

    // Slow work after the checkpoint (here the summary) is not waited for.
    let f = speaker_fixture(true);
    start_meeting(&f).await;
    let (summarizing, release) = gate(&f.models.gate);
    let stop = stop_request(&f);
    summarizing.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(
        f.backend.wait_for_saved_transcript(),
        MeetingSave::Transcript
    );
    release.send(()).unwrap();
    assert_eq!(
        stop.join().unwrap().status,
        500,
        "abandoned after the quick exit"
    );
    assert!(!f.dir.path().join("meetings").exists());
}

#[tokio::test]
async fn a_worker_model_load_failure_shows_live_as_unavailable() {
    let f = speaker_fixture(true);
    f.capture.speaker_load_fail.store(true, Ordering::SeqCst);
    let start = request(
        f.app(),
        "POST",
        "/meeting/start",
        Some(json!({"title":"Speakers", "auto_title":false})),
    )
    .await;
    // The start never waits for the model load: either state is possible here.
    assert!(
        matches!(
            start.1["diarization"].as_str(),
            Some("collecting" | "unavailable")
        ),
        "{}",
        start.1
    );
    let mut live = Value::Null;
    for _ in 0..500 {
        live = request(f.app(), "GET", "/meeting/status", None).await.1["session"].clone();
        if live["diarization"] == "unavailable" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(live["diarization"], "unavailable", "{live}");
    assert_eq!(
        live["diarization_error"], "speaker label models could not be loaded",
        "a fixed reason, never the backend's message"
    );
    // Stop keeps its channel fallback.
    let saved = request(f.app(), "POST", "/meeting/stop", None).await.1;
    assert_eq!(saved["diarization"], "failed");
    assert_eq!(saved["speakers"], json!([]));
    let log = fs::read_to_string(saved["log_path"].as_str().unwrap()).unwrap();
    assert!(log.contains("**Others:** sounds good, Dana owns the blog post"));
}

#[tokio::test]
async fn sidecar_seconds_are_rounded_like_the_stop_payload() {
    let f = speaker_fixture(true);
    // Turns of 1.5 s - 0.123 s: 1.377 s of talk each.
    f.capture.turn_trim_ms.store(123, Ordering::SeqCst);
    let saved = stopped_meeting(&f).await;
    assert_eq!(saved["diarization"], "ok", "{saved}");
    let record = sidecar(&f, saved["meeting_id"].as_str().unwrap());
    for (stored, public) in record["speakers"]
        .as_array()
        .unwrap()
        .iter()
        .zip(saved["speakers"].as_array().unwrap())
    {
        assert_eq!(stored["seconds"], 1.4, "{stored}");
        assert_eq!(stored["seconds"], public["seconds"]);
    }
}

#[tokio::test]
async fn opting_in_after_the_meeting_started_keeps_its_voice_data_off_disk() {
    let mut f = speaker_fixture(true);
    start_meeting(&f).await; // recognition off at start
    f.config.set_field("speaker_recognition", json!(true));
    let saved = request(f.app(), "POST", "/meeting/stop", None).await;
    assert_eq!(saved.0, 200, "{}", saved.1);
    assert_eq!(saved.1["diarization"], "ok");
    let record = sidecar(&f, saved.1["meeting_id"].as_str().unwrap());
    for sp in record["speakers"].as_array().unwrap() {
        assert!(
            sp["centroid"].is_null() && sp["voiceprint_id"].is_null(),
            "{sp}"
        );
    }
    assert!(
        !f.dir.path().join("speakers").exists(),
        "no registry read or write"
    );
}

#[tokio::test]
async fn each_start_takes_the_live_diarization_settings_despite_a_stale_request_config() {
    let mut f = speaker_fixture(true);
    f.config.set_field("diarization_backend", json!("mock"));
    // The live config file; `None` = unreadable.
    let live: Arc<Mutex<Option<Config>>> = Arc::new(Mutex::new(Some(f.config.clone())));
    let reader = live.clone();
    f.backend = Arc::new(
        MeetingsBackend::new()
            .unwrap()
            .with_capture(f.capture.clone())
            .with_providers(Arc::new(Providers(f.models.clone())))
            .with_queue(PromptQueue::new(f.dir.path().join("queue.json")))
            .with_history_path(f.dir.path().join("history.sqlite"))
            .with_speaker_data(Arc::new(SpeakerData::with_roots(
                f.dir.path().join("meetings"),
                f.dir.path().join("speakers"),
            )))
            .with_config_reader(Arc::new(move || {
                reader
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("unreadable"))
            })),
    );
    let set_live = |field: &str, value: Value| {
        live.lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .set_field(field, value);
    };
    let centroids = |f: &Fixture, saved: &Value| -> Vec<bool> {
        sidecar(f, saved["meeting_id"].as_str().unwrap())["speakers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|sp| !sp["centroid"].is_null())
            .collect()
    };
    let started = |f: &Fixture| {
        let app = f.app();
        async move {
            let start = request(
                app,
                "POST",
                "/meeting/start",
                Some(json!({"title":"Speakers", "auto_title":false})),
            )
            .await;
            assert_eq!(start.0, 200, "{}", start.1);
            start.1
        }
    };
    let stopped = |f: &Fixture| {
        let app = f.app();
        async move {
            let saved = request(app, "POST", "/meeting/stop", None).await;
            assert_eq!(saved.0, 200, "{}", saved.1);
            saved.1
        }
    };

    // Opted in via the file; the request snapshot still says off.
    assert_eq!(f.config.field_bool("speaker_recognition"), Some(false));
    set_live("speaker_recognition", json!(true));
    assert_eq!(started(&f).await["diarization"], "collecting");
    let saved = stopped(&f).await;
    assert_eq!(saved["diarization"], "ok", "{saved}");
    assert!(centroids(&f, &saved).iter().any(|c| *c), "{saved}");

    // Opted out again; the request snapshot now says on.
    f.config.set_field("speaker_recognition", json!(true));
    set_live("speaker_recognition", json!(false));
    started(&f).await;
    let saved = stopped(&f).await;
    assert_eq!(saved["diarization"], "ok", "{saved}");
    assert!(centroids(&f, &saved).iter().all(|c| !*c), "{saved}");

    // Speaker labels off via the file: no collector despite the request.
    set_live("speaker_diarization", json!(false));
    assert_eq!(started(&f).await["diarization"], "off");
    assert_eq!(stopped(&f).await["diarization"], "off");

    // An unreadable live config leaves labels (and recognition) off.
    set_live("speaker_diarization", json!(true));
    *live.lock().unwrap() = None;
    assert_eq!(started(&f).await["diarization"], "off");
    assert_eq!(stopped(&f).await["diarization"], "off");

    // Readable again: the next meeting collects.
    *live.lock().unwrap() = Some(f.config.clone());
    assert_eq!(started(&f).await["diarization"], "collecting");
    assert_eq!(stopped(&f).await["diarization"], "ok");
}

#[tokio::test]
async fn a_trailing_slash_never_turns_a_delete_into_the_bulk_erase() {
    let mut f = speaker_fixture(true);
    f.config.set_field("speaker_recognition", json!(true));
    let saved = stopped_meeting(&f).await;
    let id = saved["meeting_id"].as_str().unwrap();
    let body = json!({"name":"Dana","meeting_id":id,"speaker_id":"S2"});
    assert_eq!(
        request(f.app(), "POST", "/api/speakers", Some(body))
            .await
            .0,
        201
    );
    for path in ["/api/speakers/", "/api/speakers//"] {
        assert_eq!(
            request(f.app(), "DELETE", path, None).await,
            (404, json!({"detail":"Not Found"})),
            "{path}"
        );
    }
    assert_eq!(
        request(f.app(), "GET", "/api/speakers", None).await.1["speakers"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "nothing was forgotten"
    );
    // Non-destructive slash redirects keep their Python parity.
    let response = f
        .app()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/speakers/")
                .header("Host", "localhost")
                .header("X-Localflow-Token", "test-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 307);
}

#[tokio::test]
async fn an_unavailable_embedder_never_stages_enrollment_audio() {
    let mut f = Fixture::new();
    f.config.set_field("speaker_diarization", json!(true));
    f.config.set_field("speaker_recognition", json!(true));
    // `Capture::diarizer` offers no backend (like a factory without one).
    let mut clip = 24u32.to_be_bytes().to_vec();
    clip.extend_from_slice(b"ftypM4A \0\0\0\0M4A isom");
    let mut body = b"--b\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\nAna\r\n\
                     --b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.m4a\"\r\n\r\n"
        .to_vec();
    body.extend_from_slice(&clip);
    body.extend_from_slice(b"\r\n--b--\r\n");
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "content-type",
        "multipart/form-data; boundary=b".parse().unwrap(),
    );
    let response = f.backend.handle_local(
        BackendRequest {
            method: "POST".parse().unwrap(),
            path: "/api/speakers".into(),
            query: Default::default(),
            headers,
            body: body.into(),
            config: f.config.clone(),
        },
        true,
    );
    // 503 where ffmpeg runs (the pipe yields nothing for this stub clip); 500
    // without ffmpeg. Either way the raw audio was never written to disk.
    assert!(
        response.status == 503 && response.body["detail"] == "voice model unavailable"
            || response.status == 500,
        "{} {}",
        response.status,
        response.body
    );
    assert!(!f.dir.path().join("speakers").exists());
    assert_eq!(f.capture.diarizer_calls.load(Ordering::SeqCst), 1);
}
