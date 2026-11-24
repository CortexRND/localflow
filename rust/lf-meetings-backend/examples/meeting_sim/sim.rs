//! Scripted-transcript meeting sim for the Rust runtime (eval/test only).
//!
//! The Rust counterpart of `evals/meeting_sim/driver.py`. It lives in an example
//! so no test hook ships in `lf-rs` / `lf-hub`: the seam is the crate's public
//! injection points (`CaptureFactory`, `ProviderFactory`), driven through the real
//! `lf-server` router and `MeetingsBackend` lifecycle:
//!
//! `GET /meeting/templates` → `POST /meeting/start` → scripted transcript lines
//! (one audio chunk per line, transcribed by a scripted transcriber) + a
//! mid-meeting `POST /meeting/notes` → `POST /meeting/stop` (summary against the
//! real configured LLM) → `POST /meeting/chat` per scenario question → a chat on a
//! fresh backend ("restart") that must find the saved transcript on disk.
//!
//! The result JSON has the same shape as the Python driver's, so
//! `evals/meeting_sim/gates.py` and `evals/perf/adapters/meeting_gates.py`
//! score it unchanged (plus `"runtime": "rust"`).

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    body::{to_bytes, Body},
    http::Request,
    Router,
};
use lf_core::config::Config;
use lf_meetings::{
    dispatch::PromptQueue, AudioCallback, AudioSource, SystemAudioUnavailable, Transcriber,
};
use lf_meetings_backend::{CaptureFactory, MeetingsBackend, ProviderFactory, TitleLlm};
use lf_providers::{
    factory::{self, BuildOptions},
    Health, LlmProvider, ModelInfo, ProviderError, SecretStore,
};
use lf_server::{router, ServerState};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tower::ServiceExt;

pub const DEFAULT_LLM_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_LLM_MODEL: &str = "llama3.2:3b";
const RATE: usize = 16_000;
const LINE_SECONDS: usize = 5;
const TOKEN: &str = "meeting-sim-token";
/// How long one scripted line may take to show up as a transcript segment.
const SEGMENT_TIMEOUT: Duration = Duration::from_secs(30);

pub const USAGE: &str = "usage: meeting_sim --scenario ID --out PATH [--scenarios FILE] \
[--llm-url URL] [--llm-model MODEL] [--llm-provider ollama|openai-compatible] [--sut PATH] [--cleanup]";

#[derive(Debug, Clone, PartialEq)]
pub struct Args {
    pub scenario: String,
    pub out: PathBuf,
    pub scenarios: PathBuf,
    pub llm_url: String,
    pub llm_model: String,
    pub llm_provider: String,
    pub sut: PathBuf,
    pub cleanup: bool,
}

/// Same flags as `driver.py` (`--output` is an alias of `--out`); env
/// `SIM_SUT`, `SIM_LLM_URL`, `SIM_LLM_MODEL` are the defaults, flags win.
pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut parsed = Args {
        scenario: String::new(),
        out: PathBuf::new(),
        scenarios: repo_root.join("evals/meeting_sim/scenarios.json"),
        llm_url: env("SIM_LLM_URL").unwrap_or_else(|| DEFAULT_LLM_URL.into()),
        llm_model: env("SIM_LLM_MODEL").unwrap_or_else(|| DEFAULT_LLM_MODEL.into()),
        llm_provider: "ollama".into(),
        sut: env("SIM_SUT").map(PathBuf::from).unwrap_or(repo_root),
        cleanup: false,
    };
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        if flag == "--cleanup" {
            parsed.cleanup = true;
            continue;
        }
        if flag == "-h" || flag == "--help" {
            bail!("{USAGE}");
        }
        let (flag, inline) = match flag.split_once('=') {
            Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
            None => (flag, None),
        };
        let value = match inline {
            Some(value) => value,
            None => args
                .next()
                .ok_or_else(|| anyhow!("{flag} needs a value\n{USAGE}"))?,
        };
        match flag.as_str() {
            "--scenario" => parsed.scenario = value,
            "--out" | "--output" => parsed.out = value.into(),
            "--scenarios" => parsed.scenarios = value.into(),
            "--llm-url" => parsed.llm_url = value,
            "--llm-model" => parsed.llm_model = value,
            "--llm-provider" => parsed.llm_provider = value,
            "--sut" => parsed.sut = value.into(),
            _ => bail!("unknown flag {flag}\n{USAGE}"),
        }
    }
    if parsed.scenario.is_empty() || parsed.out.as_os_str().is_empty() {
        bail!("--scenario and --out are required\n{USAGE}");
    }
    if !matches!(parsed.llm_provider.as_str(), "ollama" | "openai-compatible") {
        bail!("--llm-provider must be ollama or openai-compatible");
    }
    Ok(parsed)
}

pub fn load_scenario(path: &Path, id: &str) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let doc: Value =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    doc["scenarios"]
        .as_array()
        .and_then(|all| all.iter().find(|s| s["id"] == id))
        .cloned()
        .ok_or_else(|| anyhow!("scenario {id:?} not in {}", path.display()))
}

// --------------------------------------------------------------------------
// Scripted capture: line i is a constant tone at 0.1 + i/1000, decoded back to
// its line by the scripted transcriber (the same trick as driver.py). The idle
// source gets a quiet filler tone (transcribed to "", so no segment) instead of
// pure silence: the session skips silent chunks without calling the
// transcriber, so the filler is what lets the sim see that every chunk it fed
// has been drained before it feeds the next line. Chunks never merge, and
// segment offsets stay at 5 s per line like driver.py's.
// --------------------------------------------------------------------------

type Slot = Arc<Mutex<Option<AudioCallback>>>;
const FILLER: f32 = 0.05;

fn tone(index: usize) -> Vec<f32> {
    vec![0.1 + index as f32 * 0.001; RATE * LINE_SECONDS]
}

fn filler() -> Vec<f32> {
    vec![FILLER; RATE * LINE_SECONDS]
}

struct FeedSource {
    slot: Slot,
    label: &'static str,
}
impl AudioSource for FeedSource {
    fn start(&mut self, callback: AudioCallback) -> Result<(), SystemAudioUnavailable> {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(callback);
        Ok(())
    }
    fn stop(&mut self) -> Result<(), SystemAudioUnavailable> {
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        Ok(())
    }
    fn label(&self) -> &str {
        self.label
    }
}

struct ScriptTranscriber {
    lines: Arc<Vec<String>>,
    chunks: Arc<AtomicUsize>,
}
impl Transcriber for ScriptTranscriber {
    fn transcribe(&self, audio: &[f32], _prompt: Option<&str>) -> Result<String> {
        let _counted = Counted(&self.chunks);
        let mean = audio.iter().map(|v| f64::from(*v)).sum::<f64>() / audio.len().max(1) as f64;
        if (mean - f64::from(FILLER)).abs() < 0.0005 {
            return Ok(String::new());
        }
        let index = ((mean - 0.1) / 0.001).round();
        if index < 0.0 {
            bail!("sim: unscripted audio (mean {mean})");
        }
        self.lines
            .get(index as usize)
            .cloned()
            .ok_or_else(|| anyhow!("sim: no scripted line {index}"))
    }
}

/// Counts a chunk once the transcriber returns (the session takes a chunk out
/// of its buffer before transcribing it).
struct Counted<'a>(&'a AtomicUsize);
impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct ScriptedCapture {
    lines: Arc<Vec<String>>,
    chunks: Arc<AtomicUsize>,
    system_ok: bool,
    mic: Slot,
    others: Slot,
}
impl CaptureFactory for ScriptedCapture {
    fn transcriber(&self, _: &Config) -> Result<Arc<dyn Transcriber>> {
        Ok(Arc::new(ScriptTranscriber {
            lines: self.lines.clone(),
            chunks: self.chunks.clone(),
        }))
    }
    fn microphone(&self, _: &Config) -> Result<Box<dyn AudioSource>> {
        Ok(Box::new(FeedSource {
            slot: self.mic.clone(),
            label: "sim-mic",
        }))
    }
    fn system_audio(&self, _: &Config) -> Result<Option<Box<dyn AudioSource>>> {
        anyhow::ensure!(self.system_ok, "sim: no loopback device");
        Ok(Some(Box::new(FeedSource {
            slot: self.others.clone(),
            label: "sim-loopback",
        })))
    }
}

fn feed(slot: &Slot, audio: Vec<f32>) -> bool {
    let callback = slot.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match callback {
        Some(callback) => {
            callback(audio);
            true
        }
        None => false,
    }
}

// --------------------------------------------------------------------------
// Recording LLM: the real configured provider, timed per call.
// --------------------------------------------------------------------------

type Calls = Arc<Mutex<Vec<Value>>>;

fn ms_since(started: Instant) -> f64 {
    (started.elapsed().as_secs_f64() * 1_000_000.0).round() / 1000.0
}

fn secs(ms: f64) -> f64 {
    (ms / 10.0).round() / 100.0
}

struct RecordingLlm {
    inner: Box<dyn LlmProvider>,
    calls: Calls,
}
impl LlmProvider for RecordingLlm {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    fn offline(&self) -> bool {
        self.inner.offline()
    }
    fn complete(
        &self,
        system: &str,
        user: &str,
        max_tokens: Option<i64>,
    ) -> Result<String, ProviderError> {
        let started = Instant::now();
        let result = self.inner.complete(system, user, max_tokens);
        let ms = ms_since(started);
        let mut call = json!({"prompt": user, "secs": secs(ms), "ms": ms});
        match &result {
            Ok(out) => call["output"] = json!(out),
            Err(error) => {
                call["output"] = json!("");
                call["error"] = json!(error.to_string());
            }
        }
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(call);
        result
    }
    fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        self.inner.list_models()
    }
    fn healthcheck(&self) -> Health {
        self.inner.healthcheck()
    }
}

/// Empty secret store so the simulator never loads or forwards stored user API
/// keys to a simulation endpoint.
struct NoSecrets;
impl SecretStore for NoSecrets {
    fn get(&self, _name: &str) -> String {
        String::new()
    }
    fn set(&self, _name: &str, _value: &str) -> Result<()> {
        Ok(())
    }
    fn delete(&self, _name: &str) -> Result<()> {
        Ok(())
    }
}

const SIM_LLM_OPTIONS: BuildOptions = BuildOptions {
    timeout: Some(300),
    num_ctx: Some(8192),
    temperature: None,
    with_api_key: false,
};

/// Meeting LLM provider wired without stored credentials, with calls recorded.
struct RecordingProviders {
    calls: Calls,
}
impl ProviderFactory for RecordingProviders {
    fn llm(&self, config: &Config) -> Result<Box<dyn LlmProvider>> {
        Ok(Box::new(RecordingLlm {
            inner: factory::build_llm(config, &SIM_LLM_OPTIONS, &NoSecrets)?,
            calls: self.calls.clone(),
        }))
    }
    fn title(&self, _config: &Config) -> Option<TitleLlm> {
        None
    }
    fn work_prompts(&self, _config: &Config) -> Option<Box<dyn LlmProvider>> {
        None
    }
}

// --------------------------------------------------------------------------
// HTTP client over the real router
// --------------------------------------------------------------------------

struct Client {
    runtime: tokio::runtime::Runtime,
    http: Vec<Value>,
}
impl Client {
    fn send(
        &self,
        app: &Router,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value, f64)> {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("Host", "localhost")
            .header("Content-Type", "application/json")
            .header("X-Localflow-Token", TOKEN)
            .body(Body::from(body.map(Value::to_string).unwrap_or_default()))?;
        let started = Instant::now();
        let (status, bytes) = self.runtime.block_on(async {
            let response = app.clone().oneshot(request).await?;
            let status = response.status().as_u16();
            let bytes = to_bytes(response.into_body(), usize::MAX).await?;
            anyhow::Ok((status, bytes))
        })?;
        let ms = ms_since(started);
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
        Ok((status, body, ms))
    }
    /// A recorded call: `[status, body]` plus an entry in `http` (like driver.py's `call`).
    fn call(
        &mut self,
        app: &Router,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value, f64)> {
        let (status, body, ms) = self.send(app, method, path, body.as_ref())?;
        self.http.push(json!({"call": format!("{method} {path}"), "status": status, "secs": secs(ms), "ms": ms}));
        Ok((status, body, ms))
    }
    /// Wait until the session has transcribed `chunks` fed chunks and holds
    /// `segments` transcript segments (unrecorded polling of GET /meeting/status).
    fn wait_for_drain(
        &self,
        app: &Router,
        done: &AtomicUsize,
        chunks: usize,
        segments: usize,
    ) -> Result<()> {
        let deadline = Instant::now() + SEGMENT_TIMEOUT;
        loop {
            let (status, body, _) = self.send(app, "GET", "/meeting/status", None)?;
            let have = body["session"]["segment_count"].as_u64().unwrap_or(0) as usize;
            let drained = done.load(Ordering::SeqCst);
            if status == 200 && have >= segments && drained >= chunks {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!(
                    "timed out waiting for transcript segment {segments} \
                     (have {have}, chunks {drained}/{chunks}, status {status})"
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn pair(status: u16, body: &Value) -> Value {
    json!([status, body])
}

pub fn sim_config(work: &Path, args: &Args) -> Config {
    let mut config = Config::default();
    for (key, value) in [
        ("llm_provider", json!(args.llm_provider)),
        ("llm_base_url", json!(args.llm_url)),
        ("llm_model", json!(args.llm_model)),
        ("sounds_enabled", json!(false)),
        ("meetings_enabled", json!(true)),
        ("meeting_watch", json!(false)),
        // The session worker drains audio every second; the sim waits for each
        // line's segment before feeding the next, so lines never merge.
        ("meeting_chunk_seconds", json!(1)),
        ("work_prompts", json!(false)),
        ("capture_system_audio", json!(true)),
        ("history_enabled", json!(false)),
        ("perf_metrics", json!(false)),
        ("vault_path", json!(work.join("vault"))),
        ("meeting_templates_dir", json!(work.join("templates"))),
        ("prompts_dir", json!(work.join("prompts"))),
    ] {
        config.set_field(key, value);
    }
    config
}

fn backend(work: &Path, capture: Arc<ScriptedCapture>, calls: &Calls) -> Arc<MeetingsBackend> {
    Arc::new(
        MeetingsBackend::new()
            .expect("meetings backend")
            .with_capture(capture)
            .with_providers(Arc::new(RecordingProviders {
                calls: calls.clone(),
            }))
            .with_queue(PromptQueue::new(work.join("queue.json")))
            .with_history_path(work.join("history.sqlite")),
    )
}

fn app(config: &Config, backend: Arc<MeetingsBackend>) -> Router {
    router(Arc::new(ServerState::new(
        config.clone(),
        TOKEN.into(),
        backend,
    )))
}

fn sut_commit(sut: &Path) -> String {
    std::process::Command::new("git")
        .arg("-C")
        .arg(sut)
        .args(["rev-parse", "HEAD"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

fn str_field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| anyhow!("scenario field {key:?} must be a string"))
}

/// Run one scenario and write the result JSON to `args.out`. Returns a one-line summary.
pub fn run(args: &Args) -> Result<Value> {
    let sc = load_scenario(&args.scenarios, &args.scenario)?;
    let lines: Vec<(String, String)> = sc["lines"]
        .as_array()
        .ok_or_else(|| anyhow!("scenario lines must be a list"))?
        .iter()
        .map(|line| {
            (
                line[0].as_str().unwrap_or("").to_string(),
                line[1].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let system_ok = str_field(&sc, "system_audio")? == "ok";
    let notes_final = format!(
        "{}{}",
        str_field(&sc, "notes_final")?,
        sc["notes_final_extra"].as_str().unwrap_or("")
    );
    let work_dir = tempfile::Builder::new()
        .prefix(&format!("lfsim-rs-{}-", args.scenario))
        .tempdir()
        .context("create sim temp dir")?;
    let work = work_dir.path().to_path_buf();
    let config = sim_config(&work, args);
    let calls: Calls = Arc::default();
    let capture = Arc::new(ScriptedCapture {
        lines: Arc::new(lines.iter().map(|(_, text)| text.clone()).collect()),
        chunks: Arc::default(),
        system_ok,
        mic: Arc::default(),
        others: Arc::default(),
    });
    let mut client = Client {
        runtime: tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?,
        http: Vec::new(),
    };
    let mut res = json!({
        "scenario": args.scenario, "runtime": "rust", "commit": sut_commit(&args.sut), "started": now(),
        "notes_final": notes_final, "llm": {"url": args.llm_url, "model": args.llm_model},
        "tmp": work,
    });
    let mut timings = json!({"stop_to_notes_ms": null, "summarize_ms": null, "chat_ms": [], "restart_chat_ms": null});
    let main = backend(&work, capture.clone(), &calls);
    let server = app(&config, main.clone());

    let (st, body, _) = client.call(&server, "GET", "/meeting/templates", None)?;
    res["templates"] = pair(st, &body);
    let start = json!({"title": sc["title"], "template": sc["template"], "user_notes": sc["notes_initial"]});
    let (st, body, _) = client.call(&server, "POST", "/meeting/start", Some(start))?;
    res["start"] = pair(st, &body);
    if st != 200 {
        bail!("POST /meeting/start failed: {st} {body}");
    }
    let (mut segments, mut chunks) = (0, 0);
    for (i, (source, _)) in lines.iter().enumerate() {
        // Without system audio "others" lines are lost and only the mic advances, as in driver.py.
        let (active, idle) = if source == "me" {
            (Some(&capture.mic), &capture.others)
        } else if system_ok {
            (Some(&capture.others), &capture.mic)
        } else {
            (None, &capture.mic)
        };
        if let Some(active) = active {
            chunks += usize::from(feed(active, tone(i)));
            segments += 1;
        }
        chunks += usize::from(feed(idle, filler()));
        client.wait_for_drain(&server, &capture.chunks, chunks, segments)?;
        if i == lines.len() / 2 {
            let (st, body, _) = client.call(&server, "GET", "/meeting/status", None)?;
            res["status_mid"] = pair(st, &body);
            let (st, body, _) = client.call(
                &server,
                "POST",
                "/meeting/notes",
                Some(json!({"notes": notes_final})),
            )?;
            res["notes_save"] = pair(st, &body);
        }
    }
    let (st, stop, stop_ms) = client.call(&server, "POST", "/meeting/stop", None)?;
    res["stop"] = pair(st, &stop);
    // POST /meeting/stop summarizes and writes the notes before returning.
    timings["summarize_ms"] = json!(stop_ms);
    timings["stop_to_notes_ms"] = json!(stop_ms);
    let read = |key: &str| {
        stop[key]
            .as_str()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default()
    };
    res["notes_text"] = json!(read("notes_path"));
    res["log_text"] = json!(read("log_path"));
    res["summarize_calls"] = json!(calls.lock().unwrap_or_else(|e| e.into_inner()).len());
    let mut chats = Vec::new();
    let questions = sc["chat"].as_array().cloned().unwrap_or_default();
    for q in &questions {
        let (st, body, ms) = client.call(
            &server,
            "POST",
            "/meeting/chat",
            Some(json!({"question": q["q"]})),
        )?;
        timings["chat_ms"].as_array_mut().unwrap().push(json!(ms));
        let answer = match &body {
            Value::Object(_) => body["answer"].as_str().unwrap_or("").to_string(),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        let mut chat = q.clone();
        chat["status"] = json!(st);
        chat["answer"] = json!(answer);
        chats.push(chat);
    }
    res["chats"] = json!(chats);
    // "Restart": a fresh backend with no in-memory last meeting must find the
    // saved transcript on disk (driver.py clears server._last_saved).
    let restarted = app(&config, backend(&work, capture.clone(), &calls));
    let first = questions
        .first()
        .map(|q| q["q"].clone())
        .unwrap_or(json!("What was discussed?"));
    let (st, body, ms) = client.call(
        &restarted,
        "POST",
        "/meeting/chat",
        Some(json!({"question": first})),
    )?;
    timings["restart_chat_ms"] = json!(ms);
    res["restart"] =
        json!({"status": st, "log_path": body["log_path"], "expected": stop["log_path"]});

    let llm_calls = calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
    timings["llm_call_ms"] = json!(llm_calls
        .iter()
        .map(|c| c["ms"].clone())
        .collect::<Vec<_>>());
    timings["llm_calls_count"] = json!(llm_calls.len());
    let llm_secs: f64 = llm_calls.iter().filter_map(|c| c["secs"].as_f64()).sum();
    res["http"] = json!(client.http);
    res["llm_calls"] = json!(llm_calls);
    res["timings"] = timings;
    res["finished"] = json!(now());
    drop(server);
    drop(main);
    if args.cleanup {
        work_dir.close().context("remove sim temp dir")?;
    } else {
        let _ = work_dir.keep();
    }
    if let Some(parent) = args.out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&args.out, serde_json::to_string_pretty(&res)?)
        .with_context(|| format!("write {}", args.out.display()))?;
    Ok(json!({
        "scenario": args.scenario, "runtime": "rust", "stop": res["stop"][0],
        "llm_calls": res["timings"]["llm_calls_count"], "secs": llm_secs,
        "stop_to_notes_ms": res["timings"]["stop_to_notes_ms"],
    }))
}
